# vproc

aarch64 assembly + Rust coroutine executor that runs multiple "virtual processes" inside a single Android process.

Android 只看到 1 个进程，内部通过用户态调度器在协程之间切换上下文。

## 背景

Termux/hermux 每执行一个命令 = `fork()+exec()` = Android 多看到 1 个进程。Android 通过 `RLIMIT_NPROC` 限制每 UID 进程数（通常几百），后台进程被 cgroup 冻结，LMK 随时可能 SIGKILL。`apt install` 期间可能 fork 数十个子进程，容易触发限制。

Go 运行时在 1 个 OS 进程内调度数万个 goroutine，上下文切换仅 ~100ns。但 Go 控制所有代码的编译和执行，Termux 运行的是预编译 ELF 二进制。vproc 的目标是在不修改现有二进制的前提下，用协程替代真实进程。

## 架构

```
┌───────────────────────────────────────────────────┐
│  主进程 (Android 看到 1 个进程)                     │
│                                                   │
│  ┌─────────────────────────────────────────────┐  │
│  │  vproc Runtime (Rust + aarch64 asm)          │  │
│  │                                              │  │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────┐    │  │
│  │  │ VP 1     │ │ VP 2     │ │ VP 3     │    │  │
│  │  │ (coro)   │ │ (coro)   │ │ (coro)   │    │  │
│  │  │ own stack│ │ own stack│ │ own stack│    │  │
│  │  │ own fds  │ │ own fds  │ │ own fds  │    │  │
│  │  └────┬─────┘ └────┬─────┘ └────┬─────┘    │  │
│  │       └──────┬──────┘──────────┘           │  │
│  │     Scheduler (asm context_switch)          │  │
│  │              │                              │  │
│  │  ┌───────────┴───────────┐                  │  │
│  │  │ ELF Loader / dlopen   │                  │  │
│  │  │ Interpreter Delegation│                  │  │
│  │  │ Virtual FD Table      │                  │  │
│  │  │ Virtual Pipes         │                  │  │
│  │  │ C LD_PRELOAD .so      │                  │  │
│  │  │ Rust FFI Runtime      │                  │  │
│  │  └───────────────────────┘                  │  │
│  └─────────────────────────────────────────────┘  │
│                                                   │
│  libvproc_preload.so (LD_PRELOAD, pure C)         │
│  → dlopen("libvproc.so") → Rust FFI              │
└───────────────────────────────────────────────────┘
```

## 实现细节

### Phase 1: 协程调度器 ✅

aarch64 汇编上下文切换 + Rust 调度器。

**上下文切换** (`asm/switch.S`): 保存/恢复 callee-saved 寄存器 (x19-x28, x29/fp, x30/lr, d8-d15)，切换 sp。160 字节栈帧。

**协程创建** (`src/coroutine.rs`): 分配 2 MiB 栈，在栈顶构造 vproc_switch 帧。x19 存函数指针（通过 double-box 模式将 `Box<dyn FnOnce()>` 胖指针转为瘦指针），x30 存 trampoline 地址。首次切换到该协程时，`ret` 跳到 trampoline，trampoline 调用函数，完成后调 `vproc_exit()`。

**调度器** (`src/executor.rs`): `UnsafeCell<Executor>` 避免引用冲突（RefCell 会在 yield 嵌套调用时 panic）。HashMap 存储 coroutine，VecDeque 就绪队列，轮转调度。`schedule()` 统一处理 yield 和 exit。

**关键设计决策**:
- Double-box: `Box::into_raw(Box::new(f))` — 胖指针 16B 存不进 u64，需要套一层
- UnsafeCell 而非 RefCell — yield() 在 block_on_all() 借用内调用
- global_asm! trampoline — 用 `#[naked]` 属性在 Rust 1.95 需要 `#[unsafe(naked)]` 且只能用 `naked_asm!`，改用 global_asm! 更稳定

**性能**: 1000 协程 4001 次切换在 20ms 内完成（release），~5μs/switch（含 HashMap/VecDeque 开销）。

### Phase 2: 虚拟 fork/waitpid ✅

纯协程虚拟化，无需真实 fork。

`virtual_fork()` 创建子协程而非真实进程，`virtual_waitpid()` 通过 yield 等待子协程完成。所有父子逻辑运行在根协程内（yield 需要协程上下文）。支持嵌套 fork（子协程 → 孙协程）。

`src/preload.rs` 实现了 LD_PRELOAD 拦截层（fork/execve/waitpid/_exit），用 `VPROC=1` 环境变量开关。

### Phase 3: 用户态 ELF 加载器 ✅

加载 PIE ELF 二进制到协程内执行，替代真实 execve()。

**ELF 解析器** (`src/elf.rs`): 纯安全 Rust，零依赖。解析 Elf64_Ehdr/Phdr/Dyn/Rela 结构体，验证 magic/class/endian/machine/type。`#[repr(C, packed)]` + `read_unaligned` 避免对齐问题。

**PIE 加载器** (`src/loader.rs`):
1. 解析 ELF 头，找到 PT_LOAD 段
2. 在 0x2000000000（128 GiB 区域，Android VA 空间上限 ~512 GiB）bump 分配地址
3. mmap LOAD 段（R-X for text, RW for data），复制文件内容，BSS 零填充
4. 应用 R_AARCH64_RELATIVE 重定位（`base + addend`）

**ELF 入口跳板** (`asm/elf_entry.S`):
```asm
__vproc_elf_entry:
    mov  sp, x20    // 切换到 ELF 栈布局
    br   x19        // 跳转到入口点
```

**ELF 栈布局**:
```
sp → argc (u64)
     argv[0] ... argv[argc-1], NULL
     envp[0] ... envp[n], NULL
     auxv: {AT_PHDR, addr}, {AT_PHNUM, n}, ... {AT_NULL, 0}
```

**双路径加载**:
- **静态 PIE**: 自定义加载器（mmap + 重定位 + 跳板），在协程内直接跳转到 ELF 入口
- **动态二进制**: dlopen() 加载为共享对象，dlsym("main") 找入口，在常规协程内调用

### Phase 4: 系统调用拦截 + 虚拟 fd + 管道 ✅

拦截关键 libc 函数，让加载的二进制不会杀死宿主进程。

**退出码传播**:
- Coroutine 新增 `exit_code: i32` 字段
- `vproc_exit_with_code(code)` 设置退出码并终止协程
- `get_exit_code(pid)` 仅对已完成的协程返回 `Some(code)`，避免过早返回默认值 0
- preload 层拦截 `exit()`/`_exit()` → 调用 `vproc_exit_with_code()` 而非真实系统调用

**虚拟 fd 表** (`src/vfd.rs`):
```rust
enum Vfd {
    Real(i32),                    // 直通真实内核 fd
    PipeRead(*mut PipeBuffer),    // 管道读端
    PipeWrite(*mut PipeBuffer),   // 管道写端
}
```
- 每个虚拟进程有独立 fd 命名空间（thread-local HashMap）
- fd 0/1/2 默认直通真实 stdin/stdout/stderr
- 支持 open/close/dup/dup2 操作

**管道模拟**:
- `PipeBuffer` 64 KiB 环形缓冲区
- 读端空时 yield（协作式），写端满时 yield
- 支持协程间双向通信

**LD_PRELOAD 拦截层** (`src/preload.rs`):
- 拦截 10 个函数: exit/_exit, fork, waitpid/wait4, execve, pipe, read, write, close, dup, dup2
- 所有函数检查 `VPROC=1` 环境变量，未设置时直通真实 libc
- dlsym(RTLD_NEXT) 结果缓存，避免重复符号解析
- 未在协程上下文时（current_vpid() == None）自动回退到真实 libc

**测试结果**:
```
--- Test 1: exit code propagation ---
  [child] about to exit(42)
  [parent] child exited with 42 (expected 42) ✅

--- Test 2: virtual pipe ---
  [writer] wrote 12 bytes
  [reader] got: hello pipe! ✅
```

### Phase 5: C LD_PRELOAD 层 + Rust FFI 运行时 ✅

Rust 编译的 .so 带入 Rust stdlib（panic handler、allocator），与宿主进程 glibc/bionic 冲突导致 crash。改用纯 C .so 做 LD_PRELOAD，Rust 运行时通过 dlopen 按需加载。

**双层 .so 架构**:
```
libvproc_preload.so (LD_PRELOAD, 纯 C ~310 行)
  → dlopen("libvproc.so") 或 RTLD_DEFAULT
  → 调用 vproc_ffi_* 函数

libvproc.so (Rust cdylib, FFI 运行时)
  → 导出 14 个 vproc_ffi_* 函数
  → 包装协程调度器、虚拟 fd、管道、ELF 加载
```

**C preload 层** (`preload/preload.c`):
- `struct vproc_ffi` 一次解析所有 FFI 函数指针，`dlsym` 批量加载
- 先尝试 `dlopen("libvproc.so")`，失败则回退 `RTLD_DEFAULT`（支持主二进制内嵌 FFI）
- 拦截 12 个 libc 函数，全部检查 `VPROC=1` 开关
- 无协程上下文时（`current_vpid() == 0`）自动直通真实 libc

**Rust FFI 层** (`src/ffi.rs`):
- 14 个 `#[no_mangle] extern "C"` 函数导出运行时能力
- `vproc_ffi_exit()` 无协程上下文时用 raw `svc #0` 终止进程（避免 LD_PRELOAD 递归）
- 支持 virtual fd 操作：`pipe`, `is_virtual_fd`, `read`, `write`, `close`, `dup`, `dup2`

**E2E 测试** (`examples/e2e_preload.rs`):
```
Test 1: exit(42) interception     — 协程内 libc::exit(42) 被拦截，exit code 42 捕获 ✅
Test 2: multiple coroutines       — 两个协程分别 exit(10)/exit(20) ✅
Test 3: _exit(99) interception    — libc::_exit(99) 也被正确拦截 ✅
switches: 7, exit: 0
```

### Phase 6: 解释器委托 ✅

加载任意预编译动态 ELF 二进制，无需 `-rdynamic` 或符号导出。

**问题**: Phase 3 的 dlopen 路径要求 `dlsym("main")`，但 Termux 预编译二进制（`apt`, `dpkg`, `ls`, `true`）不导出 `main` 符号。

**方案**: `dlopen` + 入口点跳转。

```
1. 解析 ELF 头获取 e_entry（入口偏移）
2. dlopen() 加载二进制 → 动态链接器加载所有 DT_NEEDED 依赖
3. dl_iterate_phdr() 找到加载基址（canonicalize 解析符号链接）
4. 计算入口地址 = base + e_entry
5. 清空 DT_INIT_ARRAY/DT_INIT（防止 dlopen 已执行的构造器被 _start 再次调用）
6. Coroutine::new_elf() 构造协程栈（argc/argv/envp/auxv）
7. 跳转到入口点 → _start → __libc_init → main() → exit()
```

**关键实现细节** (`src/vexec.rs`):
- `find_loaded_base()`: 先 `canonicalize()` 解析符号链接（`true` → `coreutils`），再用 `dl_iterate_phdr` 按路径匹配基址
- `clear_init_arrays()`: 遍历 PT_DYNAMIC 中的 DT_INIT_ARRAY/DT_INIT 条目，将 d_val 清零。`#[repr(C, packed)]` 结构体用 `addr_of_mut!` + `write_unaligned` 避免对齐 UB
- `virtual_execve()` 调度：优先 via_entry，失败回退 dlsym("main")

**验证** — 加载真实 Termux 预编译二进制:
```
true  → exit code 0 ✅
false → exit code 1 ✅
echo  → exit code 0 ✅
```

## 当前状态

### 已验证
- ✅ 协程调度器（1000 协程，~5μs/switch）
- ✅ 虚拟 fork/waitpid（嵌套 fork，0 真实进程）
- ✅ PIE ELF 加载（静态 + 动态二进制）
- ✅ 退出码传播（exit() 不杀进程）
- ✅ 虚拟 fd 表 + 管道模拟
- ✅ 纯 C LD_PRELOAD .so（避免 Rust stdlib 冲突）
- ✅ 解释器委托（加载任意预编译动态二进制）

### 待解决

1. **hermux 集成**: 将 vproc 的 `virtual_execve_via_entry()` 接入 hermux 命令执行流程
2. **更多 syscall 拦截**: signal (kill/sigaction), socket, 文件操作 (open/stat/access), 进程管理 (getpid/getppid)
3. **虚拟 fork**: 协程栈复制 + fd 表复制（当前 fork 返回 ENOSYS）
4. **静态 PIE raw syscall**: 直接 `svc #0` 系统调用无法拦截
5. **端到端验证**: `apt install` 完整工作流

## 文件结构

```
vproc/
├── Cargo.toml
├── build.rs                  # 编译 asm/*.S
├── asm/
│   ├── switch.S              # aarch64 上下文切换 (160B 帧)
│   └── elf_entry.S           # ELF 入口跳板
├── preload/
│   ├── preload.c             # 纯 C LD_PRELOAD 层 (310 行)
│   ├── Makefile              # 构建 libvproc_preload.so + 测试
│   └── test_preload.c        # 基础拦截测试
├── src/
│   ├── lib.rs                # 公开 API + c_array_to_vec 工具函数
│   ├── coroutine.rs          # Coroutine struct + trampoline + new_elf() + exit_code
│   ├── executor.rs           # UnsafeCell 调度器 + spawn_elf() + vproc_exit_with_code()
│   ├── elf.rs                # ELF64 解析器 (纯安全 Rust)
│   ├── ffi.rs                # C FFI 接口 (14 个 vproc_ffi_* 导出函数)
│   ├── loader.rs             # PIE 加载器 (mmap + 重定位) + build_auxv()
│   ├── vexec.rs              # virtual_execve (static + dlsym + via_entry)
│   ├── vfd.rs                # 虚拟 fd 表 + 环形缓冲区管道
│   ├── preload.rs            # Rust LD_PRELOAD 拦截层 (10 函数)
│   └── arch/
│       ├── mod.rs
│       └── aarch64.rs        # context_switch FFI
├── examples/
│   ├── basic.rs              # 3 协程交替
│   ├── stress.rs             # 1000 协程压力测试
│   ├── fork_sim.rs           # 虚拟 fork/waitpid
│   ├── vexec_demo.rs         # 静态 PIE 加载演示
│   ├── vexec_dynamic_demo.rs # dlopen 动态加载演示
│   ├── pipe_demo.rs          # 退出码传播 + 虚拟管道演示
│   ├── e2e_preload.rs        # C preload + Rust runtime E2E 测试
│   └── entry_demo.rs         # 解释器委托演示 (加载 Termux 二进制)
└── tests/
    ├── test_static_pie.S     # 最小 aarch64 静态 PIE (write+exit)
    └── test_fork.c           # fork/waitpid 拦截测试
```

## 构建

```bash
cargo build --release
cargo test
cargo run --example entry_demo      # 加载 Termux 二进制
cd preload && make e2e              # C preload E2E 测试
```

需要 aarch64 Linux/Android 环境。

## 关联

- [hermux issue #526](https://github.com/Bahtya/hermux/issues/526) — 原始提案和进展跟踪
- [Go runtime](https://go.dev/) — goroutine M:N 调度启发
- [gVisor](https://gvisor.dev/) — 用户态内核参考
- [Graphene](https://grapheneproject.github.io/) — Library OS 参考
