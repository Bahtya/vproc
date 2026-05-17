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
│  │  │ Virtual FD Table      │                  │  │
│  │  │ Virtual Pipes         │                  │  │
│  │  │ LD_PRELOAD Intercepts │                  │  │
│  │  └───────────────────────┘                  │  │
│  └─────────────────────────────────────────────┘  │
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

## 当前工作重心

### 已验证
- ✅ 协程调度器（1000 协程，20ms）
- ✅ 虚拟 fork/waitpid（嵌套 fork，0 真实进程）
- ✅ PIE ELF 加载（静态 + 动态二进制）
- ✅ 退出码传播（exit(42) 不杀进程）
- ✅ 虚拟 fd 表 + 管道模拟

### 待解决的关键问题

1. **LD_PRELOAD exit() 拦截**: 当前需要 `LD_PRELOAD=libvproc.so VPROC=1` 才能拦截 dlopen 二进制的 exit()。需要 C/NDK 编译生产 .so（Rust stdlib 与 glibc 冲突）
2. **动态二进制符号导出**: dlopen 路径要求 `-rdynamic` 编译。预编译二进制不满足，需要完整解释器委托
3. **静态 PIE 的 raw syscall**: 直接 `svc #0` 系统调用无法拦截（无 seccomp/ptrace）

### 下一步方向

- **C/NDK preload .so**: 用 C 编译 LD_PRELOAD 层，避免 Rust stdlib 冲突
- **解释器委托**: 加载 ld-linux/linker64 并跳转，支持任意预编译动态二进制
- **更多 syscall**: signal/pipe/socket 的用户态实现
- **完整 apt install 工作流**: 端到端验证

## 文件结构

```
vproc/
├── Cargo.toml
├── build.rs                  # 编译 asm/*.S
├── asm/
│   ├── switch.S              # aarch64 上下文切换 (160B 帧)
│   └── elf_entry.S           # ELF 入口跳板
├── src/
│   ├── lib.rs                # 公开 API: spawn, yield, block_on_all, exit, get_exit_code
│   ├── coroutine.rs          # Coroutine struct + trampoline + new_elf() + exit_code
│   ├── executor.rs           # UnsafeCell 调度器 + spawn_elf() + vproc_exit_with_code()
│   ├── elf.rs                # ELF64 解析器 (纯安全 Rust)
│   ├── loader.rs             # PIE 加载器 (mmap + 重定位)
│   ├── vexec.rs              # virtual_execve API (static + dynamic)
│   ├── vfd.rs                # 虚拟 fd 表 + 环形缓冲区管道
│   ├── preload.rs            # LD_PRELOAD 拦截层 (10 函数)
│   └── arch/
│       ├── mod.rs
│       └── aarch64.rs        # context_switch FFI
├── examples/
│   ├── basic.rs              # 3 协程交替
│   ├── stress.rs             # 1000 协程压力测试
│   ├── fork_sim.rs           # 虚拟 fork/waitpid
│   ├── vexec_demo.rs         # 静态 PIE 加载演示
│   ├── vexec_dynamic_demo.rs # dlopen 动态加载演示
│   └── pipe_demo.rs          # 退出码传播 + 虚拟管道演示
└── tests/
    ├── test_static_pie.S     # 最小 aarch64 静态 PIE (write+exit)
    └── test_fork.c           # fork/waitpid 拦截测试
```

## 构建

```bash
cargo build --release
cargo test
cargo run --example vexec_demo
cargo run --example pipe_demo
```

需要 aarch64 Linux/Android 环境。

## 关联

- [hermux issue #526](https://github.com/Bahtya/hermux/issues/526) — 原始提案和进展跟踪
- [Go runtime](https://go.dev/) — goroutine M:N 调度启发
- [gVisor](https://gvisor.dev/) — 用户态内核参考
- [Graphene](https://grapheneproject.github.io/) — Library OS 参考
