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
│  │  └────┬─────┘ └────┬─────┘ └────┬─────┘    │  │
│  │       └──────┬──────┘──────────┘           │  │
│  │     Scheduler (asm context_switch)          │  │
│  │              │                              │  │
│  │  ┌───────────┴───────────┐                  │  │
│  │  │ ELF Loader / dlopen   │                  │  │
│  │  │ virtual_execve        │                  │  │
│  │  └───────────────────────┘                  │  │
│  └─────────────────────────────────────────────┘  │
└───────────────────────────────────────────────────┘
```

## 实现细节

### Phase 1: 协程调度器

aarch64 汇编上下文切换 + Rust 调度器。

**上下文切换** (`asm/switch.S`): 保存/恢复 callee-saved 寄存器 (x19-x28, x29/fp, x30/lr, d8-d15)，切换 sp。160 字节栈帧。

**协程创建** (`src/coroutine.rs`): 分配 2 MiB 栈，在栈顶构造 vproc_switch 帧。x19 存函数指针（通过 double-box 模式将 `Box<dyn FnOnce()>` 胖指针转为瘦指针），x30 存 trampoline 地址。首次切换到该协程时，`ret` 跳到 trampoline，trampoline 调用函数，完成后调 `vproc_exit()`。

**调度器** (`src/executor.rs`): `UnsafeCell<Executor>` 避免引用冲突（RefCell 会在 yield 嵌套调用时 panic）。HashMap 存储 coroutine，VecDeque 就绪队列，轮转调度。`schedule()` 统一处理 yield 和 exit。

**关键设计决策**:
- Double-box: `Box::into_raw(Box::new(f))` — 胖指针 16B 存不进 u64，需要套一层
- UnsafeCell 而非 RefCell — yield() 在 block_on_all() 借用内调用
- global_asm! trampoline — 用 `#[naked]` 属性在 Rust 1.95 需要 `#[unsafe(naked)]` 且只能用 `naked_asm!`，改用 global_asm! 更稳定

**性能**: 1000 协程 4001 次切换在 20ms 内完成（release），~5μs/switch（含 HashMap/VecDeque 开销）。

### Phase 2: 虚拟 fork/waitpid

纯协程虚拟化，无需真实 fork。

`virtual_fork()` 创建子协程而非真实进程，`virtual_waitpid()` 通过 yield 等待子协程完成。所有父子逻辑运行在根协程内（yield 需要协程上下文）。支持嵌套 fork（子协程 → 孙协程）。

`src/preload.rs` 实现了 LD_PRELOAD 拦截层（fork/execve/waitpid/_exit），用 `VPROC=1` 环境变量开关。当前验证了纯 Rust 协程路径；生产环境 LD_PRELOAD .so 需要用 C/NDK 编译（Rust stdlib 与 glibc 冲突）。

### Phase 3: 用户态 ELF 加载器

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

vproc_switch 恢复帧后，x30 指向此跳板，x19 = 入口地址，x20 = ELF 数据区 sp。

**ELF 栈布局**:
```
sp → argc (u64)
     argv[0] ... argv[argc-1], NULL
     envp[0] ... envp[n], NULL
     auxv: {AT_PHDR, addr}, {AT_PHNUM, n}, ... {AT_NULL, 0}
```

**双路径加载**:
- **静态 PIE**: 自定义加载器（mmap + 重定位 + 跳板），在协程内直接跳转到 ELF 入口
- **动态二进制**: dlopen() 加载为共享对象，dlsym("main") 找入口，在常规协程内调用（需要 -rdynamic 编译以导出符号）

## 当前工作重心

### 待解决的关键问题

1. **系统调用拦截**: 加载的二进制调用 `_exit()` 会终止真实进程。Phase 4 需要拦截系统调用（seccomp-bpf 或 LD_PRELOAD 扩展），让 `_exit()` 变为协程终止
2. **动态二进制符号导出**: dlopen 路径要求二进制用 `-rdynamic` 编译。大部分预编译二进制不满足。需要实现完整解释器委托（加载 ld-linux/linker64 并跳转）
3. **生产 LD_PRELOAD .so**: 用 C + Android NDK 编译（而非 Rust stdlib），避免 glibc/bionic 冲突

### Phase 4 路线

- **系统调用模拟**: signal/pipe/socket 的用户态实现
- **fd 表隔离**: 每个虚拟进程独立的文件描述符表
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
│   ├── lib.rs                # 公开 API: spawn, yield, block_on_all
│   ├── coroutine.rs          # Coroutine struct + trampoline + new_elf()
│   ├── executor.rs           # UnsafeCell 调度器 + spawn_elf()
│   ├── elf.rs                # ELF64 解析器 (纯安全 Rust)
│   ├── loader.rs             # PIE 加载器 (mmap + 重定位)
│   ├── vexec.rs              # virtual_execve API
│   ├── preload.rs            # LD_PRELOAD 拦截层
│   └── arch/
│       ├── mod.rs
│       └── aarch64.rs        # context_switch FFI
├── examples/
│   ├── basic.rs              # 3 协程交替
│   ├── stress.rs             # 1000 协程压力测试
│   ├── fork_sim.rs           # 虚拟 fork/waitpid
│   ├── vexec_demo.rs         # 静态 PIE 加载演示
│   └── vexec_dynamic_demo.rs # dlopen 动态加载演示
└── tests/
    ├── test_static_pie.S     # 最小 aarch64 静态 PIE (write+exit)
    └── test_fork.c           # fork/waitpid 拦截测试
```

## 构建

```bash
cargo build --release
cargo test
cargo run --example vexec_demo
```

需要 aarch64 Linux/Android 环境。

## 关联

- [hermux issue #526](https://github.com/Bahtya/hermux/issues/526) — 原始提案和进展跟踪
- [Go runtime](https://go.dev/) — goroutine M:N 调度启发
- [gVisor](https://gvisor.dev/) — 用户态内核参考
- [Graphene](https://grapheneproject.github.io/) — Library OS 参考
