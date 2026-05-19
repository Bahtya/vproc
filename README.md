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

### Phase 7: 虚拟 fork ✅

在协程内实现 `fork()` 语义，子进程获得父进程栈的完整副本，无需创建真实 OS 进程。

**问题**: `apt`/`dpkg` 大量使用 `fork()` 创建子进程处理下载、解压、配置。没有虚拟 fork，所有依赖 fork 的程序都无法运行。

**方案**: helper 协程 + 栈复制。

```
1. vproc_ffi_fork() 被 preload fork() 调用
2. 检查 is_fork_child 标志 → 子进程直接返回 0
3. 父进程: spawn helper 协程
4. do_yield() → vproc_switch 保存 callee-saved 寄存器到父进程栈
5. helper 调用 spawn_fork_child():
   - fork_from() 复制父进程整个栈（含保存的寄存器帧）
   - 设置 is_fork_child=true, 复制 fd 表
   - 记录父子关系到 Executor.children
6. helper 退出 → 调度器切回父进程
7. 父进程读 fork_child_pid → 返回 child_pid
8. 子进程被调度时恢复到 do_yield() 之后 → fork_child_pid=0 → 返回 0
```

**关键实现**:

`Coroutine::fork_from()` (`src/coroutine.rs`):
- 分配新栈（与父进程相同大小）
- `ptr::copy_nonoverlapping` 复制整个栈内容
- 子进程 sp 在相同偏移位置（`child_base + (parent_sp - parent_base)`）
- 设置 `ppid`, `is_fork_child`, `fork_child_pid=0`

`Executor::spawn_fork_child()` (`src/executor.rs`):
- 封装 pid 分配 + 栈复制 + 队列插入 + children 追踪 + fd 表复制
- `children: HashMap<VPid, Vec<VPid>>` 追踪父子关系
- `is_child_of()`, `reap_child()` 辅助 waitpid

`VfdTable::clone_for_fork()` (`src/vfd.rs`):
- Real fd: 直接复制（共享内核 file description，匹配 Linux fork 语义）
- Pipe fd: 共享同一个 PipeBuffer 指针（匹配 Linux pipe 语义）

**安全限制** — caller-saved 寄存器丢失:

`vproc_switch` 只保存 callee-saved 寄存器（x19-x30, d8-d15）。x0-x18 中的变量在子进程中值未定义。这对 fork-then-execve 模式安全（子进程只读 fork 返回值 x0=0 后立即调 execve），但长时间运行的子进程需要注意。

**验证**:
```
--- Test 1: fork -> _exit(42) -> waitpid ---
  [parent] child vpid = 3
  [child] exiting with 42
  [parent] child exited with 42 (expected 42) ✅

--- Test 2: 3 sequential fork children ---
  [parent] child A vpid = 5
  [child A] exiting with 10
  [parent] child B vpid = 7
  [child B] exiting with 20
  [parent] child C vpid = 9
  [child C] exiting with 30
  [parent] child A exited with 10 (expected 10) ✅
  [parent] child B exited with 20 (expected 20) ✅
  [parent] child C exited with 30 (expected 30) ✅
switches: 15
```

### Phase 8: exit() 拦截修复 + 端到端验证 ✅

修复 ELF 协程不执行的核心 bug，实现 `sh -c "echo hello"` 端到端通过。

**问题**: dlopen 的 ELF 二进制的 `_start` → `__libc_init` → `exit()` 路径中，exit() 终止了整个进程而非切回主协程。

**根因分析** (3 路并行调试):
1. `__libc_init` 重新初始化 TLS → executor 的 thread-local 状态丢失
2. `preload.rs` 的 `enabled()` 使用 `std::env::var("VPROC")`（Rust TLS 依赖），TLS 重初始化后失效
3. `enabled()` 在 `set_var("VPROC","1")` 之前被首次调用（通过 `println!` → `write()` 链），缓存了 `false`

**修复**:
- `enabled()` 改用 `libc::getenv()`（C 级别，不受 TLS 重初始化影响）
- 不缓存 `false` 结果，每次检查直到发现 `VPROC=1`
- 全局 `AtomicPtr<Executor>` 替代 TLS（存活于 `__libc_init` 重初始化）
- libc `exit()` inline hook（aarch64 trampoline 跳转到 Rust 拦截器）

**验证**:
```
=== sh -c "echo hello" test ===
hello
[parent] shell exited with 0
=== done ===

entry_demo: true→0, false→1, echo→0 ✅
```

### Phase 9: 集成、测试、清理、扩展 ✅

4 路并行推进。

**Track 1 — hermux 集成原型**: 创建了 3 个文件:
- `vproc_wrapper.h` — FFI 声明 + dlopen 运行时解析器
- `termux_vproc.c` — termux.c 替代版，运行时分发到 real fork 或 vproc 协程
- `Android.mk.vproc` — 构建配置
- 关键设计: PTY 保持真实 fd，Java 层无需改动，需新 FFI 函数 `vproc_ffi_create_process()`

**Track 2 — 复杂场景测试** (38 用例):
- ✅ exit code 传播 (0/1/42/255/true/false): 6/6 通过
- ✅ 基本 shell 命令: 3/6 通过
- ✅ 并发执行 (3 线程): 3/3 通过
- ✅ 复杂 shell (for/&&/||/变量/算术/引用): 11/12 通过
- ✅ 多命令会话 (export/子shell/分号): 10/11 通过
- ❌ 管道 `|` 和命令替换 `$()`: 5 个失败（虚拟 fork+pipe 路径不完整）

**Track 3 — 代码清理**:
- 删除 `patch_got_entries()` 死代码 124 行
- 修复 3 个编译器警告 → **0 warnings**
- 所有回归测试通过

**Track 4 — syscall 拦截扩展**:
- 新增 `kill(pid, sig)` 拦截（虚拟 pid 返回 0，真实 pid 透传）
- 新增 `getpgid(pid)` / `setpgid(pid, pgid)` stub
- 新增 `raise(sig)` 透传
- Rust + C 双层同步更新

### Phase 10: 管道修复 + 连续 execve ✅

修复管道操作符 `|` 导致的 SIGSEGV 和同一二进制连续 `virtual_execve` 挂起，解除阶段 9 识别的两个关键阻塞问题。

#### 问题 1: 管道 SIGSEGV (issue #1, PR #2)

`sh -c "echo hello | cat"` 触发 SIGSEGV。

**根因**: 虚拟 fork（协程栈复制）+ 虚拟 pipe（内存环形缓冲区）无法满足 shell 的真实 pipe/fork 语义。bionic 的 `waitpid()` 内部调用 `wait4()`，而我们也拦截了 `wait4()`，形成 `waitpid → libc waitpid → libc wait4 → 我们的 wait4 → 我们的 waitpid` 无限递归 → 栈溢出 → SIGSEGV。

**修复**:
- **真实 fork**: `fork()` 拦截器改用 `dlsym(RTLD_NEXT)` 获取真正的 libc fork（`libc::fork()` 会解析到我们自己的符号），子进程设 `REAL_FORK_CHILD` 标志
- **真实 pipe**: `pipe()` 始终创建真实 OS 管道
- **raw wait4 系统调用**: 绕过 libc 直接用 `syscall(__NR_wait4)` 等待真实子进程，消除递归
- **直通模式**: `REAL_FORK_CHILD` 的所有拦截器（read/write/close/dup/dup2/exit/_exit/execve）直通真实 libc

**验证** — 8 项集成测试全部通过:
```
test_simple_echo .............. ok   (echo hello)
test_sequential_builtins ...... ok   (echo hello; echo world)
test_pipe_builtin_builtin ..... ok   (echo hello | true)
test_pipe_builtin_cat ......... ok   (echo hello | cat)
test_multi_stage_pipe ......... ok   (echo hello world | cat | cat)
test_subshell_pipe ............ ok   ((echo a; echo b) | cat)
test_pipe_with_grep ........... ok   (printf 'foo\nbar\nbaz\n' | grep ba)
```

#### 问题 2: 连续 virtual_execve 挂起 (issue #3, PR #4)

同一进程内第二次调用 `virtual_execve_via_entry("sh", ...)` 挂起。

**根因**:
1. `extract_main_addr()` 的 LDR 指令掩码 `0xFFC003FF` 检查 Rn=0，但 dash 的 `_start` 用 `ldr x2, [x2, #off]`（Rn=2），导致 main() 地址提取失败
2. 即使提取成功，直接调用 `main()` 时 shell 的全局变量（job table、fd tracking 等）残留第一次运行的脏状态，初始化挂死

**修复**:
- **放宽 LDR 掩码**: `0xFFC003FF` → `0xFFC0001F`，只检查 Rt=2 不检查 Rn
- **寄存器配对**: 改为从 `ldr x2, [xN, #imm]` 反向搜索匹配的 `adrp xN`，支持 `adrp+add` 指令对
- **可写段快照/恢复**: `BINARY_CACHE` 保存 main 地址 + 所有可写 PT_LOAD 段的完整内容。后续调用先恢复段数据再调 main()，模拟 execve 的"干净进程映像"语义
- **GOT re-patch**: 段恢复后重新执行 `patch_got_for_loaded_binary()`，确保拦截器在 `__libc_init` 可能的 GOT 修改后仍然生效
- **原始权限恢复**: `WritableSegment` 记录 PT_LOAD flags，restore 时恢复正确权限而非硬编码 RWX
- **DlHandle 保留**: cache entry 保存 dlopen handle，为未来多 binary 卸载做准备

**验证**:
```
test_sequential_pipe_invocations ... ok  (echo hello | cat → echo world | cat)
```

### Phase 11: 基础设施健壮化 ✅

PR #6, #9, #10 — 资源管理、文件系统拦截、信号、per-coroutine cwd、二进制缓存生命周期。

**资源管理** (PR #6):
- `Coroutine::mapped_regions` — 协程退出时自动 munmap 加载的 ELF 段
- `Coroutine::c_strings` — 协程退出时释放 C 字符串内存
- `VfdTable::Drop` — 自动关闭所有虚拟管道，标记 EOF
- `remove_table_and_get_fds()` — 协程退出时关闭真实内核 fd，Arc 去重避免 double-close

**文件系统拦截** (PR #6):
- `open()` → 拦截，创建 `Vfd::File(Arc<FileRef>)` 真实 fd 映射
- `fstat()` → 拦截，透传到真实 fd
- `lseek()` → 拦截，透传到真实 fd
- `close-on-exec` — `VfdTable::close_cloexec()` execve 时关闭标记的 fd

**信号 + per-coroutine cwd** (PR #9):
- `Coroutine::pending_signals` — 每协程信号队列
- `Executor::deliver_signals()` — 调度前投递，SIGKILL/SIGTERM 立即终止协程
- `Coroutine::cwd` — 每协程工作目录，`chdir`/`getcwd` 虚拟化
- `SIGPIPE` — pipe 写端关闭后写操作触发 SIGPIPE 信号投递

**二进制缓存生命周期** (PR #10):
- `BinaryCacheEntry::active_users: AtomicU32` — 引用计数跟踪
- `Coroutine::binary_path` — 记录协程使用的二进制路径
- `track_binary_user()` — spawn 时递增 refcount
- `release_binaries()` — 协程退出时递减，zero-users 自动 dlclose
- `DlGuard` RAII — 防止 error path 的 dlopen handle 泄漏
- `reap_done_coroutines()` — 统一清理：children map、fd table、mapped regions、binary cache

### Phase 12: hermux 集成 FFI ✅

PR #13 — 多会话并发安全（Issue #11, #12）。

**Spawn Queue 架构**:
```
Java Thread A ──→ SPAWN_QUEUE ──→ Driver Thread ──→ Executor (exclusive)
Java Thread B ──→ SPAWN_QUEUE ──↗     │
Java Thread C ──→ WAITERS ────────── yield → check waiters → signal Condvar
```

- `SpawnRequest` + `SPAWN_QUEUE` — Java 线程提交 spawn 请求，不直接修改 Executor
- `DRIVER_WAKE` Condvar — 有新工作时唤醒 driver thread，空闲时阻塞（非轮询）
- `vproc_ffi_create_process()` — 提交队列请求 → 阻塞等待 Condvar → 返回 vpid
- `vproc_ffi_run_until_exit()` — 注册 waiter → 阻塞等待 Condvar → 返回 exit code
- `run_driver_loop()` — drain queue → spawn → yield → reap done coroutines → check waiters → block

**修复**:
- Issue #12: 数据竞争 — driver thread 独占 Executor，消除并发修改
- Issue #11 Bug 2: 协程泄漏 — `reap_done_coroutines()` 在 driver loop 中清理资源
- Issue #11 Bug 1: signal 118 crash — 同 #12 根因，消除数据竞争后解决

### Phase 13: CI Release ✅

PR #14, #15, #16 — 推送 `v*` tag 自动构建发布。

**GitHub Actions Workflow** (`.github/workflows/release.yml`):
- 触发: `push tags: ["v*"]`
- 构建: `nttld/setup-ndk@v1` + `cargo-ndk` + `aarch64-linux-android`
- 产出: `vproc-{version}-aarch64-linux-android.tar.gz`（内含 `libvproc.so`）
- 发布: `softprops/action-gh-release@v2` 自动创建 Release + release notes
- `build.rs` 移除 hardcoded `gcc`，让 `cc` crate 自动使用 NDK clang

## 当前状态

### 已验证
- ✅ 协程调度器（1000 协程，~5μs/switch）
- ✅ 虚拟 fork（栈复制 + fd 表复制，0 真实进程）
- ✅ waitpid（exit code 传播，父子关系追踪）
- ✅ PIE ELF 加载（静态 + 动态二进制）
- ✅ 退出码传播（exit() 不杀进程）
- ✅ 虚拟 fd 表 + 管道模拟
- ✅ 纯 C LD_PRELOAD .so（避免 Rust stdlib 冲突）
- ✅ 解释器委托（加载任意预编译动态二进制）
- ✅ **端到端验证**: `sh -c "echo hello"` 完整工作流
- ✅ 信号拦截（kill/getpgid/setpgid/raise）
- ✅ 代码清理（0 compiler warnings）
- ✅ **管道操作符 `|`**: 真实 fork + 真实 pipe + raw wait4（8 项集成测试）
- ✅ **同一二进制连续 execve**: 可写段快照/恢复 + GOT re-patch
- ✅ **多阶段管道**: `echo hello | cat | cat` 三级管道通过
- ✅ **子 shell 管道**: `(echo a; echo b) | cat` 通过
- ✅ **hermux 集成 FFI**: `create_process` + `run_until_exit`（spawn queue 并发安全）
- ✅ **多会话并发**: driver thread 独占 Executor，Java 线程通过队列提交
- ✅ **文件系统拦截**: open/fstat/lseek + `Vfd::File` fd 表管理
- ✅ **per-coroutine cwd**: `chdir`/`getcwd` 虚拟化
- ✅ **信号传递**: SIGPIPE/SIGKILL/SIGTERM 虚拟化
- ✅ **二进制缓存生命周期**: 引用计数 + auto-dlclose
- ✅ **CI Release**: 推送 `v*` tag 自动构建并发布 `libvproc.so`
- ✅ **40 项集成测试**: pipe/file/cwd/signal/stress 全部通过

### 待解决

1. **setsid/作业控制** (Issue #8): 拦截 setsid/getpgrp/tcsetpgrp 返回虚拟值，或回退真实 fork
2. **静态 PIE raw syscall**: 直接 `svc #0` 系统调用无法拦截

## 文件结构

```
vproc/
├── Cargo.toml
├── build.rs                  # 编译 asm/*.S
├── .github/workflows/
│   └── release.yml           # CI: tag 触发构建 + GitHub Release
├── asm/
│   ├── switch.S              # aarch64 上下文切换 (160B 帧)
│   └── elf_entry.S           # ELF 入口跳板
├── preload/
│   ├── preload.c             # 纯 C LD_PRELOAD 层 (~320 行)
│   ├── Makefile              # 构建 libvproc_preload.so + 测试
│   └── test_preload.c        # 基础拦截测试
├── src/
│   ├── lib.rs                # 公开 API + c_array_to_vec 工具函数
│   ├── coroutine.rs          # Coroutine struct + trampoline + new_elf() + fork_from()
│   ├── executor.rs           # UnsafeCell 调度器 + reap_done_coroutines()
│   ├── elf.rs                # ELF64 解析器 (纯安全 Rust)
│   ├── ffi.rs                # C FFI 接口 + spawn queue + driver thread
│   ├── loader.rs             # PIE 加载器 (mmap + 重定位) + build_auxv()
│   ├── vexec.rs              # virtual_execve + binary cache lifecycle
│   ├── vfd.rs                # 虚拟 fd 表 + 环形缓冲区管道 + clone_for_fork()
│   ├── preload.rs            # Rust LD_PRELOAD 拦截层 (18 函数)
│   └── arch/
│       ├── mod.rs
│       └── aarch64.rs        # context_switch FFI
├── examples/
│   ├── basic.rs              # 3 协程交替
│   ├── stress.rs             # 1000 协程压力测试
│   ├── fork_sim.rs           # 虚拟 fork/waitpid
│   ├── fork_exec_demo.rs     # 虚拟 fork + exit + waitpid 演示
│   ├── vexec_demo.rs         # 静态 PIE 加载演示
│   ├── vexec_dynamic_demo.rs # dlopen 动态加载演示
│   ├── pipe_demo.rs          # 退出码传播 + 虚拟管道演示
│   ├── e2e_preload.rs        # C preload + Rust runtime E2E 测试
│   ├── entry_demo.rs         # 解释器委托演示 (加载 Termux 二进制)
│   ├── sh_test.rs            # sh -c "echo hello" 端到端测试
│   └── test_single.rs        # 测试工具，支持多命令顺序执行
└── tests/
    ├── pipe_tests.rs         # 管道 + 连续 execve 集成测试 (8 项)
    ├── infra_tests.rs        # 基础设施测试 (8 项)
    ├── file_tests.rs         # 文件系统测试 (8 项)
    ├── cwd_tests.rs          # 工作目录测试 (4 项)
    ├── signal_tests.rs       # 信号测试 (3 项)
    ├── stress_tests.rs       # 压力测试 (6 项)
    ├── test_static_pie.S     # 最小 aarch64 静态 PIE (write+exit)
    └── test_fork.c           # fork/waitpid 拦截测试
```

## 构建

```bash
cargo build --release
cargo run --example sh_test --release      # sh -c "echo hello" 端到端
cargo run --example entry_demo --release   # 加载 Termux 二进制
cargo run --example fork_exec_demo --release  # 虚拟 fork 演示
cd preload && make e2e                     # C preload E2E 测试
```

需要 aarch64 Linux/Android 环境。

### 预编译 Release

从 [GitHub Releases](https://github.com/Bahtya/vproc/releases) 下载 `vproc-{version}-aarch64-linux-android.tar.gz`：

```bash
tar -xzf vproc-v0.1.0-aarch64-linux-android.tar.gz
# 得到 libvproc.so
```

推送 `v*` tag 自动触发 CI 构建并发布。

## 关联

- [hermux issue #526](https://github.com/Bahtya/hermux/issues/526) — 原始提案和进展跟踪
- [Go runtime](https://go.dev/) — goroutine M:N 调度启发
- [gVisor](https://gvisor.dev/) — 用户态内核参考
- [Graphene](https://grapheneproject.github.io/) — Library OS 参考
