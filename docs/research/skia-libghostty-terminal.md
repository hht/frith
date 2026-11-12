# 调研：Rust + winit + Skia (skia-safe) + libghostty-vt 构建 GPU 终端/多路复用器的可行性

调研日期 2026-11-12。只用一手来源：官方文档、官方仓库的源码和头文件、维护者发布的 release notes。凡是推断都标「推断」，没核对过的都标「未核实」。
版本基线：libghostty-vt **1.0.1**（tag `vt-1.0.1`）、skia-safe **0.157.0**（Skia m157）、winit **0.30.13 stable / 0.31.0-beta.4**。

## 结论

- **可行，不需要改上游。** libghostty-vt 1.0 的 C API 已覆盖全部 TUI 关键能力：alt screen、鼠标 1000/1002/1003/1006/1016、bracketed paste、focus、Kitty keyboard、键鼠编码器、2026 同步输出、OSC 8/52/9/777/133、Kitty graphics、grapheme/宽字符、scrollback、reflow。它还提供带脏行跟踪和 row id 的 render state，自绘渲染器可以直接用。PTY、窗口、剪贴板、字体渲染和 IME 都由我们自己负责。
- Skia 这一侧，backdrop blur（`SaveLayerRec::backdrop` + `image_filters::blur`）和 SkSL `RuntimeEffect` 在 skia-safe 里都有。**Neovide 已经用 skia-safe + winit 做过「浮窗模糊背后网格文字」**，和我们要的毛玻璃浮动 pane 是同一种做法。
- 主要风险不在 TUI 正确性，在工程面：
  1. **Rust 绑定的 1.0 版本尚未发布到 crates.io**，结构体是 `!Send`，没法直接拿来做多线程；
  2. 构建需要 **Zig 0.17**，而且 Windows 预编译静态库不兼容 MSVC `link.exe`；
  3. Skia 的预编译包一律带 PDF/JPEG。`textlayout`（ICU）让 macOS 压缩包大约从 4.8 MB 涨到 17.8 MB，最终可执行文件会多多少未核实；
  4. 毛玻璃的 GPU 开销没有一手数据；
  5. winit 的 IME 在 macOS、X11 上还有一批未关闭的 CJK 相关 bug。
- **建议**：先做 2–3 天 spike，测量后面 spike 计划里列的指标，再决定是否定栈。

---

## 1. libghostty-vt 的 TUI 覆盖面（1.0 C API）

总体定位：「A GhosttyTerminal is a whole terminal emulator … It doesn't draw and it doesn't talk to a pty. Both of those are yours.」（[terminal.h](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/include/ghostty/vt/terminal.h)）。发布博文也称 C API 是「a complete terminal emulator」（[libghostty 1.0 博文](https://mitchellh.com/writing/libghostty-1-0)）。

| 能力 | 在 1.0 C API 中？ | 依据 |
|---|---|---|
| Alternate screen（47/1047/1049） | 是 | `GHOSTTY_MODE_ALT_SCREEN*`（[modes.h](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/include/ghostty/vt/modes.h)）；`GHOSTTY_TERMINAL_SCREEN_ALTERNATE`（terminal.h） |
| 鼠标追踪 9/1000/1002/1003，格式 1005/1006 SGR/1015/1016 SGR-Pixels，1007 alt scroll | 是 | modes.h；鼠标编码器支持「X10, UTF-8, SGR, URxvt and SGR-Pixels」（[Input guide](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/dist/doxygen/pages/guides/input.md)） |
| Bracketed paste 2004，含 unsafe-paste 拒绝和 Kitty paste events 5522 | 是 | `ghostty_terminal_paste()` 自动读取模式（Input guide） |
| Focus events 1004 | 是，不过 mode 要自己查 | `ghostty_focus_encode()`「doesn't look at the terminal, so it's up to you to check the mode first」（Input guide） |
| Kitty keyboard protocol（5 个 flag） | 是 | `GHOSTTY_KITTY_KEY_*`（[key/encoder.h](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/include/ghostty/vt/key/encoder.h)） |
| **键鼠编码成 PTY 字节** | **库负责** | 「These functions produce those bytes」；同步模式用 `ghostty_key_encoder_set_from_terminal()` / `ghostty_mouse_encoder_set_from_terminal()`，写入后或编码前要重新同步一次（Input guide）。macOS Option-as-Alt 每次同步后都要重设（同上）。鼠标编码器需要我们提供像素尺寸和按键按住状态（同上） |
| 同步输出 2026 | 是 | 由 `GHOSTTY_TERMINAL_OPT_RENDER_HOLD` 回调通知；库**没有时钟**，超时要我们自己做（文档建议 1 s）（[Rendering guide](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/dist/doxygen/pages/guides/rendering.md)） |
| OSC 8 超链接 | 是 | cell/row 上有 `HAS_HYPERLINK` 标志（[screen.h](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/include/ghostty/vt/screen.h)），URI 通过 `ghostty_grid_ref_hyperlink_uri()` 获取（[grid_ref.h](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/include/ghostty/vt/grid_ref.h)） |
| OSC 52 剪贴板 | 是，以回调形式 | `CLIPBOARD_WRITE` / `CLIPBOARD_READ`，也覆盖 OSC 1337 Copy 和 OSC 5522。**回调是同步的**，必须在返回前 reply（[Effects guide](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/dist/doxygen/pages/guides/effects.md)） |
| OSC 9 / 777 通知 | 是 | `DESKTOP_NOTIFICATION`；另外有 OSC 9;4 进度（`PROGRESS_REPORT`）和 OSC 7501 程序状态（Effects guide） |
| OSC 133 shell integration | 是 | `SEMANTIC_PROMPT` 回调；选区 API 可以按命令输出选择（Effects guide；[vt.h 示例列表](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/include/ghostty/vt.h)） |
| OSC 0/2 标题、OSC 7 pwd、DA/XTVERSION/DSR 等查询 | 是 | 19 个 effect 的完整表见 Effects guide |
| Kitty graphics | 是（默认开启，存储上限 10 MB） | 「Today that means the Kitty graphics protocol」。**库里不带 PNG 解码器**，要通过 `ghostty_sys_set(GHOSTTY_SYS_OPT_DECODE_PNG)` 自己注入（[Images guide](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/dist/doxygen/pages/guides/images.md)）。博文说「Wuffs for image decoding in the Kitty graphics protocol」，Wuffs 也确实在[依赖清单](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/deps/libghostty-vt-third-party.json)里，但它实际负责解码哪一部分（zlib 还是 PNG）未核实。以 1.0.1 的 guide 为准，PNG 要自带 |
| Sixel | **否** | Images guide 只列了 Kitty。头文件里 sixel 只作为 DA 特性常量 `GHOSTTY_DA_FEATURE_SIXEL` 出现（[device.h](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/include/ghostty/vt/device.h)）。iTerm2 inline image 同样不在列表里 |
| Grapheme clustering / CJK 宽字符 | 是 | cell 有 wide / spacer 属性（screen.h），可以拿到完整 grapheme codepoints（[render.h](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/include/ghostty/vt/render.h)）。mode 2027 grapheme cluster 默认值未设为 true（[modes.zig](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/src/terminal/modes.zig)）。`ghostty_unicode_codepoint_width()` / `grapheme_width()` 与终端使用同一张宽度表，文档明确说可以拿来排 IME preedit（[unicode.h](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/include/ghostty/vt/unicode.h)） |
| Scrollback（含空闲压缩） | 是 | terminal.h；[Scrollback guide](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/dist/doxygen/pages/guides/scrollback.md) |
| 尺寸变化时 reflow | 是 | 「The primary screen will reflow content if wraparound mode is enabled; the alternate screen does not reflow」。resize 还会结束 2026 hold，并发送 mode 2048 尺寸报告（terminal.h `ghostty_terminal_resize`） |
| 搜索、选区（含手势）、snapshot 序列化 | 是（可选 feature） | [features.md](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/dist/doxygen/pages/features.md) |

**只在完整 Ghostty 里、1.0 C API 没有的部分**：渲染器、字体栅格化和 shaping、窗口和 surface 嵌入、PTY、配置系统。Ghostling 的 README 原话是「It contains no renderer drawing or windowing code; the consumer … provides its own」（[ghostling](https://github.com/ghostty-org/ghostling)）。

可选 feature（`-Dvt-features`）在编译期裁掉后，对应函数会返回 `GHOSTTY_UNSUPPORTED`，ABI 保持不变（features.md）。

## 2. 自绘渲染用的 render-state API

- **每帧的读取流程**：
  1. 用 `ghostty_render_state_update(state, terminal)` 拍一份快照；
  2. 用 row iterator / cell iterator 读 codepoints、grapheme、style、颜色；
  3. 另外读 cursor（含 `wide_tail`）、调色板和选区。

  行和 cell 数据只在下一次 update 之前有效（render.h；Rendering guide）。
- **脏跟踪有两层**：全局 `DIRTY_FALSE/PARTIAL/FULL`，以及每行一个 dirty flag。update **只会置位，不会清除**，画完以后要调用 `ghostty_render_state_clean()` 自己清（Rendering guide）。
- **Row id**：row id 跟随行内容移动，可以作为 shaped text 或纹理缓存的 key。dirty 判定偏保守，例如「every row is currently marked dirty after the viewport scrolls」（Rendering guide）。所以平滑滚动场景需要 overscan（`GHOSTTY_RENDER_STATE_OPT_OVERSCAN`）配合 row id 缓存（同上）。
- **Kitty 图片**：图片不跟随 cell，要单独读 image placement iterator，每个 placement 带源矩形、目标像素尺寸、layer（背景下 / 文字下 / 文字上）和 z-index（Images guide）。
- **线程规则**：「The library creates no threads … calls on one handle are never thread-safe. But calls across multiple handles are」。回调在触发它的那个线程上同步执行（[conventions.md](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/dist/doxygen/pages/conventions.md)）。
  - 推荐的模式是 IO 线程持有 terminal。渲染线程只在 `ghostty_render_state_begin_update()` 期间加锁；`end_update()` 只碰 render state 自己的内存，可以在锁外执行（render.h；Rendering guide）。
  - `ghostty_sys_set()` 是进程全局的，而且线程不安全（Images guide）。
  - 回调里**不能**重入写同一个 terminal（Effects guide）。
- **PTY I/O**：库不管。字节由我们喂进 `ghostty_terminal_vt_write()`（terminal.h；Effects guide）。
- **回写响应**（DA、DSR、DECRPM、尺寸报告、粘贴字节等）全部经过 `GHOSTTY_TERMINAL_OPT_WRITE_PTY` 回调交给我们写进 PTY。**不注册回调的话，查询一律没有应答**（「a program that asks a question gets no answer」，Effects guide）。

## 3. Rust 绑定与构建工具链

**绑定：[Uzaaft/libghostty-rs](https://github.com/Uzaaft/libghostty-rs)**。MIT 许可，有 `libghostty-vt-sys`（bindgen）和 `libghostty-vt`（safe wrapper）两个 crate。2026-11-11 仍有提交。它列在发布博文所链的 awesome 列表上的 Rust 绑定（[awesome-libghostty](https://github.com/Uzaaft/awesome-libghostty)）。

- **对 1.0 的跟进情况**：master 的 `build.rs` 固定在「vt-1.0.1, pinned by commit 108cacb…」（[build.rs](https://github.com/Uzaaft/libghostty-rs/blob/master/crates/libghostty-vt-sys/build.rs)）。但 crates.io 上最新版是 **0.2.2（2026-09-28，早于 1.0）**（[crates.io API](https://crates.io/api/v1/crates/libghostty-vt)），README 里也还写着「libghostty-vt is pre-1.0」。**要用 1.0 目前只能走 git 依赖。**
- **线程模型**：safe wrapper 里的 `Terminal` / `RenderState` 内部持有 `NonNull` 和裸指针，仓库里没有 `unsafe impl Send/Sync`（[terminal.rs](https://github.com/Uzaaft/libghostty-rs/blob/master/crates/libghostty-vt/src/terminal.rs)、[alloc.rs](https://github.com/Uzaaft/libghostty-rs/blob/master/crates/libghostty-vt/src/alloc.rs)）。
  - 推断：这两个类型因此都是 `!Send`。C 层支持的「IO 线程 + 渲染线程两阶段 update」在 safe API 下没法直接用，需要自己写 newtype 并 `unsafe impl Send`，或者直接调 `-sys`。
- **构建**：
  - 「Every method that builds the library needs Zig 0.17 … The build stops with an error on any other minor version」（[installing.md](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/dist/doxygen/pages/getting-started/installing.md)）。
  - libghostty-rs 同样要求 PATH 里有 Zig 0.17.x，默认在构建时拉取对应源码，也支持离线构建（`GHOSTTY_SOURCE_DIR` / `GHOSTTY_ZIG_SYSTEM_DIR`）。默认**静态**链接 `libghostty-vt.a`，打开 `link-dynamic` 后改为动态链接（[libghostty-rs README](https://github.com/Uzaaft/libghostty-rs)）。
- **官方预编译产物**（发布在 `release.files.ghostty.org/libghostty-vt/VERSION/`）：
  - Windows x86_64 / arm64 zip，里面有 DLL、import lib、静态 lib，ABI 为 gnu；
  - macOS / iOS **XCFramework，只含静态库**；
  - WASM。
  - **Linux 和 FreeBSD 不提供预编译**（installing.md；[PACKAGING-LIBGHOSTTY.md](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/PACKAGING-LIBGHOSTTY.md)）。
- **链接限制**：
  - Windows 的预编译静态库「works with clang, mingw and Zig, but not with MSVC's link.exe … With MSVC, use the DLL」（installing.md）。DLL 只依赖 ntdll、kernel32 和 UCRT（[README-windows.md](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/dist/libghostty-vt/README-windows.md)）。
  - 反例：herdr 用 Zig `-Dtarget=x86_64-windows-msvc` 从源码构建，再在 `*-windows-msvc` 目标上静态链接（[herdr build.rs](https://github.com/herdrdev/herdr/blob/master/crates/ghostty-vt/build.rs)）。也就是说，从源码构建时 MSVC 下静态链接走得通。
  - libghostty-rs 自己的 macOS 动态链接检查依赖 Nix 包，因为「the XCFramework contains no shared library」（libghostty-rs README）。
- **ABI 承诺**：1.0 起遵循 semver，并保证严格的 ABI 兼容。所有对象都是不透明句柄，旧版库遇到新 key 会返回 `GHOSTTY_UNSUPPORTED`（[博文](https://mitchellh.com/writing/libghostty-1-0)）。**Zig 版本不在 ABI 承诺范围内**（installing.md）。

## 4. Skia（rust-skia / skia-safe）

- **版本与维护**：
  - skia-safe 0.157.0 于 2026-11-03 发布，对齐 Skia m157。2026 年内连续发了 0.153 → 0.156 → 0.157（[releases](https://github.com/rust-skia/rust-skia/releases)；[0.157.0 notes](https://github.com/rust-skia/rust-skia/releases/tag/0.157.0)）。
  - 许可为 MIT，2026-11-10 仍有提交。
  - Neovide 锁定在 `=0.156.0`（[Neovide Cargo.toml](https://github.com/neovide/neovide/blob/main/Cargo.toml)）。
- **预编译二进制**：build.rs 会从 [skia-binaries](https://github.com/rust-skia/skia-binaries/releases) 下载，覆盖：
  - Windows x64 / arm64 msvc；
  - Linux x64 / arm64 gnu，要求 glibc ≥ 2.31；
  - macOS x64 / arm64。

  没有现成组合时需要从源码构建（ninja、python、clang 等）（[rust-skia README](https://github.com/rust-skia/rust-skia/blob/0.157.0/README.md)）。
  - **预编译包只提供固定的 feature 组合。** 0.157.0 共 152 个资产，其中桌面三平台的包**全部带 `pdf` 和 jpeg 编解码**（以[资产列表](https://github.com/rust-skia/skia-binaries/releases/tag/0.157.0)为准）。skia-safe 的默认 feature 本来就含 `pdf`、`jpeg`（[skia-safe Cargo.toml](https://github.com/rust-skia/rust-skia/blob/0.157.0/skia-safe/Cargo.toml)）。想去掉它们来缩小体积，就得从源码构建。
- **GPU 后端**：
  - Ganesh：Vulkan / OpenGL / Metal / Direct3D 都有。
  - Graphite：只有 Metal / Vulkan 的 context 创建，backend 类型包装还没做完。
  - Dawn/WebGPU：不支持。

  （[skia-safe README](https://github.com/rust-skia/rust-skia/blob/0.157.0/skia-safe/README.md)）
  - Skia 上游 m157 的 Graphite 后端目录只有 `dawn/ mtl/ vk/`，Ganesh 则有 `d3d/ gl/ mtl/ vk/`（[graphite](https://github.com/google/skia/tree/chrome/m157/src/gpu/graphite)、[ganesh](https://github.com/google/skia/tree/chrome/m157/src/gpu/ganesh)）。所以 **Windows 原生 D3D 只能用 Ganesh**。
  - Graphite 的预编译包没有带 `textlayout` 的组合（资产列表）。
  - 结论：选 Ganesh，各平台为 Metal / D3D12 / Vulkan 或 GL。rust-skia 自带的 `metal-window`、`d3d-window`、`vulkan-window`、`gl-window` 示例都基于 winit 0.30（README；Cargo.toml 的 dev-deps）。
- **Backdrop blur**：
  - skia-safe 有 `SaveLayerRec::backdrop()`、`backdrop_tile_mode()` 和 `image_filters::blur`（[canvas.rs](https://github.com/rust-skia/rust-skia/blob/0.157.0/skia-safe/src/core/canvas.rs)、[image_filters.rs](https://github.com/rust-skia/rust-skia/blob/0.157.0/skia-safe/src/effects/image_filters.rs)）。
  - 语义上，backdrop 会把当前层拷进新层，再按 clip 用滤镜处理（[SkCanvas.h m157](https://github.com/google/skia/blob/chrome/m157/include/core/SkCanvas.h)）。
  - Neovide 的浮窗模糊正是 `SaveLayerRec::default().backdrop(&blur).bounds(..)`（[rendered_layer.rs](https://github.com/neovide/neovide/blob/main/src/renderer/rendered_layer.rs)）。
  - **GPU 开销**：没有找到 Skia 官方给出的数字，**未核实**，需要在 spike 里实测。推断：开销随模糊区域面积和 sigma 增长，而且背后终端每帧有变化时，每帧都要重新做一遍。
- **SkSL**：
  - `RuntimeEffect::make_for_shader` / `make_for_color_filter` / `make_for_blender`，以及 `image_filters::runtime_shader`，都已暴露（[runtime_effect.rs](https://github.com/rust-skia/rust-skia/blob/0.157.0/skia-safe/src/effects/runtime_effect.rs)、image_filters.rs）。
  - 不支持在 Rust 里子类化 RuntimeEffect，只能当句柄用（skia-safe README），不影响写 SkSL。
- **文本**：
  - `textlayout` feature 提供 skshaper（HarfBuzz + ICU）和 skparagraph（skia-safe README）。
  - 不开它也能用 `FontMgr::match_family_style_character` 做字体回退（[font_mgr.rs](https://github.com/rust-skia/rust-skia/blob/0.157.0/skia-safe/src/core/font_mgr.rs)），用 `TextBlob` / `TextBlobBuilder::alloc_run_pos(_h)` 按 cell 定位字形（[text_blob.rs](https://github.com/rust-skia/rust-skia/blob/0.157.0/skia-safe/src/core/text_blob.rs)）。
  - **终端网格需要 shaping 吗？** 纯 ASCII 和 CJK 单字形按 cell 放置就够了。连字、组合字符、emoji ZWJ、复杂文字需要 shaping。Ghostty 本身就带 CoreText 和 HarfBuzz shaper（[src/font/shaper](https://github.com/ghostty-org/ghostty/tree/v1.4.0/src/font/shaper)）。
  - SkParagraph 是富文本段落排版，不适合网格（推断）。
  - Neovide 的做法是用 swash 做 shaping，再生成 Skia `TextBlob` 放进 LRU 缓存（[caching_shaper.rs](https://github.com/neovide/neovide/blob/main/src/renderer/fonts/caching_shaper.rs)），这样就避开了 ICU。
- **网格绘制方案**：
  - **`drawTextBlob`**：最简单，Skia Ganesh 内部有 glyph atlas（[GrAtlasManager](https://github.com/google/skia/tree/chrome/m157/src/gpu/ganesh/text)）。
  - **`drawAtlas`**：自己维护 glyph atlas，每个 cell 一个 RSXform 和一个颜色（[SkCanvas.h drawAtlas 文档](https://github.com/google/skia/blob/chrome/m157/include/core/SkCanvas.h)；skia-safe `Canvas::draw_atlas`）。这和 Ghostty 自有渲染器的 atlas + cell 思路一致（[Atlas.zig](https://github.com/ghostty-org/ghostty/blob/v1.4.0/src/font/Atlas.zig)）。

  两者谁快，**没有一手数据，需要 spike 实测**。推荐的结构是按 row id 缓存每行的 blob 或指令。
- **二进制体积**：**最终链接后的可执行文件大小没有一手来源，未核实。** 唯一的一手数据是预编译**压缩包**的大小，它不等于最终体积：
  - aarch64-apple-darwin 的 `ganesh-metal-pdf` 包约 4.8 MB，`ganesh-metal-pdf-textlayout` 约 17.8 MB；
  - x86_64-windows `d3d-ganesh-pdf` 约 10.2 MB，加上 textlayout 约 25.7 MB（资产列表）。
  - 由此可见 textlayout（含 ICU 数据，`embed-icudtl` 默认开启）是最大的增量。

## 5. winit IME

- **API**：
  - 0.30.x 用 `set_ime_allowed` / `set_ime_cursor_area`。
  - 0.31 beta 把它们标为 deprecated，改为 `request_ime_update(ImeRequest::{Enable,Update,Disable})`，配合 `ImeCapabilities` 和 `ImeRequestData::with_cursor_area`。
  - 平台支持：iOS / Android / Web / Orbital 不支持 cursor area。macOS 必须开启 IME 才能拿到死键合成的文本；X11 开启 IME 后，compose 期间不再上报死键（[winit-core window.rs @v0.31.0-beta.4](https://github.com/rust-windowing/winit/blob/v0.31.0-beta.4/winit-core/src/window.rs)）。
  - 事件形态是 `Ime::Preedit(text, cursor)` 和 `Ime::Commit`（[event.rs](https://github.com/rust-windowing/winit/blob/v0.31.0-beta.4/winit-core/src/event.rs)）。
- **各平台实现**：
  - Wayland 走 `zwp_text_input_v3`（[text_input/mod.rs](https://github.com/rust-windowing/winit/blob/v0.31.0-beta.4/winit-wayland/src/seat/text_input/mod.rs)）；
  - X11 走 XIM（[x11 ime](https://github.com/rust-windowing/winit/blob/v0.31.0-beta.4/winit-x11/src/ime/mod.rs)）；
  - macOS 是 NSTextInputClient；
  - Windows 是 IMM（推断，依据是目录结构和 issue 描述，未逐一读源码）。
- **beta.4 修复的 IME 问题**：Windows surrogate pair 下的 preedit 光标偏移、Wayland seat 变化导致 IME 失效、macOS 关闭 IME 时丢弃 marked text、不再在 Commit 前发空 Preedit（[0.31.0-beta.4 notes](https://github.com/rust-windowing/winit/releases/tag/v0.31.0-beta.4)）。beta.3 修了 macOS IME 锁死和 emoji 偏移导致的 panic（[beta.3 notes](https://github.com/rust-windowing/winit/releases/tag/v0.31.0-beta.3)）。**这些修复都不在 0.30.13 stable 里**，推断：做 CJK 实际上要用 0.31 beta。
- **截至 2026-11-12 仍未关闭的 CJK 相关 issue**（[issue 搜索](https://github.com/rust-windowing/winit/issues?q=is%3Aopen+IME)）：
  - macOS：
    - [#4807](https://github.com/rust-windowing/winit/issues/4807)：Kotoeri 汉字转换无法提交；
    - [#4524](https://github.com/rust-windowing/winit/issues/4524)：韩文 hanja 候选窗打不开；
    - [#3095](https://github.com/rust-windowing/winit/issues/3095)：韩文字符分离；
    - [#3814](https://github.com/rust-windowing/winit/issues/3814)：中文标点行为反了；
    - [#3617](https://github.com/rust-windowing/winit/issues/3617)：NSTextInputClient 忽略 selected/replacement range。
  - X11：
    - [#4777](https://github.com/rust-windowing/winit/issues/4777)：移动 cursor area 导致乱码；
    - [#3092](https://github.com/rust-windowing/winit/issues/3092)：无法禁用 IME 或设置 IME 位置；
    - [#4525](https://github.com/rust-windowing/winit/issues/4525)：Fcitx5 下 release 事件泄漏。
  - Windows：
    - [#2569](https://github.com/rust-windowing/winit/issues/2569)：MS 日文 IME 重新启用后位置失效；
    - [#2780](https://github.com/rust-windowing/winit/issues/2780)：搜狗候选窗位置不对；
    - [#4508](https://github.com/rust-windowing/winit/issues/4508)：IME 开启时仍会发出 KeyboardInput。
  - 通用：[#4412](https://github.com/rust-windowing/winit/issues/4412)：新 IME API 限制过多。
- **终端侧的配合**：preedit 由我们自己画，宽度用 `ghostty_unicode_*` 计算（unicode.h）。合成期间的按键用 `ghostty_key_event_set_composing()` 标记（[key/event.h](https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/include/ghostty/vt/key/event.h)）。
- **Kitty keyboard 需要的字段**：winit 的 `KeyEvent` 提供 `physical_key`、`key_without_modifiers`、`text_with_all_modifiers` 和 release 事件，这些在桌面平台上都已实现（event.rs）。
- **旁证**：Rio 维护了 winit 的 fork「rio-window … a fork of Winit maintained only for Rio terminal」（[rio-window](https://github.com/raphamorim/rio/tree/main/rio-window)），fork 的原因未核实。Alacritty 用的是上游 winit 0.30（[alacritty Cargo.toml](https://github.com/alacritty/alacritty/blob/master/alacritty/Cargo.toml)）。

## 6. 先例

- **Skia 绘制终端或网格的先例**：[Neovide](https://github.com/neovide/neovide)，Neovim GUI，MIT。技术栈是 skia-safe 0.156 + winit 0.30.13，后端为 macOS Metal、Windows D3D、Linux GL。它有浮窗 backdrop blur，用 swash shaping 并缓存 TextBlob（Cargo.toml；rendered_layer.rs；caching_shaper.rs）。它不是 VT 终端，但网格渲染、模糊和 IME 这些问题与我们同构，是最接近的参照。**其他基于 Skia 的终端模拟器我没有找到一手来源，未核实。**
- **基于 libghostty-vt 自绘渲染器的先例**：
  - [Ghostling](https://github.com/ghostty-org/ghostling)：官方示例，Raylib 2D 渲染，单线程；
  - libghostty-rs 的 `ghostling_rs`（macroquad）；
  - awesome 列表里的 GPUI 应用，如 shellwing、moai-studio，以及 Godot 引擎里的 Godotty（[awesome-libghostty](https://github.com/Uzaaft/awesome-libghostty)，只读了描述，渲染细节未核实）；
  - JetBrains IDE 终端，被博文列为 embedder（[博文](https://mitchellh.com/writing/libghostty-1-0)），渲染实现未核实。
- **cmux**：Swift/AppKit 的 macOS 应用，「uses libghostty as a library for terminal rendering」，用的是 Ghostty 自带的 GPU 渲染器，并且以 submodule 引入 fork [manaflow-ai/ghostty](https://github.com/manaflow-ai/ghostty)（[cmux README](https://github.com/manaflow-ai/cmux)、[.gitmodules](https://github.com/manaflow-ai/cmux/blob/main/.gitmodules)）。**它不是 libghostty-vt + 自绘渲染器的先例。**
- 非 Skia 的参照：[Rio](https://github.com/raphamorim/rio) 用 wgpu 30 + 自 fork 的 winit（Rio Cargo.toml）；Alacritty 用 winit + glutin/OpenGL。

### herdr 的技术与图形显示

**(a) 架构：跑在宿主终端里的 TUI，不是独立 GUI。**
- 「one rust binary, no electron — runs in whatever terminal you already use」（[herdr README](https://github.com/herdrdev/herdr)），Apache-2.0。
- 采用 client/server 结构：server 持有 PTY 和 pane（`portable-pty` 的 vendored 版本），client 是 TUI。官方说法是「migrating toward a server-owned runtime protocol with the TUI as one client」（[AGENTS.md](https://github.com/herdrdev/herdr/blob/master/AGENTS.md)）。
- UI 用 Rust + **ratatui 0.30 + crossterm 0.29**（[Cargo.toml](https://github.com/herdrdev/herdr/blob/master/Cargo.toml)）。
- 每个 pane 持有一个 `RenderState`，在 `src/pane/terminal.rs` 里调用 `render_state.update(terminal)`。
- 输出时对帧做 diff，生成 ANSI，用 CUP/SGR 写回宿主终端，并用 `CSI ? 2026` 包住整帧（[render_ansi.rs](https://github.com/herdrdev/herdr/blob/master/src/protocol/render_ansi.rs)）。
- libghostty-vt 的使用方式：没用 libghostty-rs，而是自己写的 `crates/ghostty-vt` 绑定。它从 `vendor/libghostty-vt` 源码构建，版本是 `1.3.2-HEAD+44f2a44df`，是 1.0 tag 之前的快照，需要 **Zig 0.16**，并带一个本地补丁，用来暴露 modifyOtherKeys 状态（[vendor json](https://github.com/herdrdev/herdr/blob/master/vendor/libghostty-vt.vendor.json)、[patches.md](https://github.com/herdrdev/herdr/blob/master/vendor/libghostty-vt.patches.md)、[build.rs](https://github.com/herdrdev/herdr/blob/master/crates/ghostty-vt/build.rs)）。
- 链接方式是静态链接，Windows 上为 `ghostty-vt-static`（build.rs）。

**(b) 图形：不自己画像素，只透传和重编码 Kitty graphics 到宿主终端。**
- 「Applications and plugins integrate through the standard Kitty graphics protocol emitted in pane terminal output. Herdr renders these images by default in compatible outer terminals」（[configuration.mdx「Kitty graphics」](https://github.com/herdrdev/herdr/blob/master/docs/next/website/src/content/docs/configuration.mdx)）。
- 由 libghostty-vt 解析 pane 输出里的 Kitty 图片，herdr 再按自己的布局重新生成 placement，发给外层终端（`src/kitty_graphics/surface.rs`、`output.rs`）。
- 被弹窗或菜单遮住时，图片会被**裁切**；每帧的分片数超出预算时，整张图隐藏（surface.rs 注释）。文档补充说「Images do not dim with dialog backgrounds」。
- 本地 Unix 下，如果宿主是 Ghostty、WezTerm 或 kitty，会用临时文件传输（`t=t`）。判断依据是 `TERM_PROGRAM`/`TERM`/`KITTY_WINDOW_ID`；SSH、tmux、screen 下禁用这条路径（[handshake.rs](https://github.com/herdrdev/herdr/blob/master/src/client/handshake.rs)、[frame_output.rs](https://github.com/herdrdev/herdr/blob/master/src/client/frame_output.rs)）。
- **Sixel、iTerm2 inline image 都不支持**：源码里「sixel」只出现在生成的 FFI bindings 中，`1337;File` 在全仓没有命中（对 commit `ee5bfb5` 的 grep）。没有找到 Unicode 块字符或盲文降级的证据（未发现，未核实是否完全没有）。
- 宿主终端不支持 Kitty graphics 时，就看不到图片；这一点是推断，依据是「compatible outer terminals」这句措辞。

**(c) 非终端内容：基本没有原生的非终端 UI。**
- 「Runtime action registration and native non-terminal plugin UI are not part of plugin v1」。插件 pane 本身也是终端 pane，放置方式有 `overlay/popup/split/tab/zoomed`（[plugins.mdx](https://github.com/herdrdev/herdr/blob/master/docs/next/website/src/content/docs/plugins.mdx)）。
- socket 图片 overlay API 已经移除，「there is no replacement socket image API」（configuration.mdx）。
- 所以 markdown 和 diff 只能交给 pane 里的 TUI 程序去渲染。herdr 自己的 UI（侧栏、状态、对话框）是 ratatui 的单元格控件。
- 这些都受**宿主终端的单元格网格和它支持的协议**限制：没有亚像素排版，没有模糊、着色器这类效果，图片能否显示也取决于外层终端。
- 对我们的意义：herdr 证明了「libghostty-vt + Rust + agent 多路复用」可行；而我们想要的毛玻璃浮层和自定义 shader，只有做 GUI 并自绘才能实现，这一点正是我们和 herdr 的区别。

## 7. 判断

**可行。** TUI 正确性方面，libghostty-vt 已经包办了解析、状态、键鼠编码和查询应答，正确性风险已大幅转移到「回调和线程接线」以及「输入事件映射」上。视觉效果方面，skia-safe 提供的能力足够，而且有 Neovide 这个同构先例。

### 风险排序

1. **Rust 绑定成熟度和线程模型**（高）
   - crates.io 版本早于 1.0，safe 类型是 `!Send`，需要自己封装多线程或者回落到 `-sys`。
   - 回调同步执行且不可重入，OSC 52 读剪贴板的回调也是同步的。要弹出征询用户的对话框，就得把 IO 线程阻塞住，或者直接拒绝。
2. **毛玻璃的 GPU 开销**（高，未核实）
   - 多个浮层叠加 backdrop blur，再遇到底层终端高频刷新（agent 流式输出），每帧都要重做模糊。
   - Retina 4K 分辨率下的帧时间和功耗未知。
3. **winit IME 的 CJK 质量**（中高）
   - 0.31 还是 beta，macOS 和 X11 上有多个未关闭的 IME bug。preedit 绘制、候选窗定位、IME 开启时的 KeyboardInput 去重都要自己处理。
4. **构建链**（中）
   - 必须锁 Zig 0.17，Linux 没有预编译。
   - Windows 的预编译 static lib 不兼容 MSVC link.exe，要改用 DLL 或者从源码 Zig 构建。
   - Skia 想去掉 pdf/jpeg 就得从源码构建，CI 时间会显著变长（推断）。
5. **二进制体积**（中，未核实）：Skia 加 textlayout/ICU 是主要增量，可以用 swash 或 rustybuzz 替代 textlayout。
6. **文本渲染质量和性能**（中）：连字、emoji、CJK 回退字体、粗细和 hinting 要与 Ghostty 观感对齐。drawTextBlob 和 drawAtlas 的取舍没有数据。
7. **图片协议**（低）：只有 Kitty，没有 sixel，Kitty 的 PNG 解码要自带。主流 agent CLI 不依赖图片协议，这一点是推断。

### Spike 计划（2–3 天）

| # | 做什么 | 测量 / 通过标准 |
|---|---|---|
| 1 | winit 0.31-beta 窗口 + skia-safe Ganesh Metal surface；接入 libghostty-rs（git 依赖，固定 vt-1.0.1）+ `portable-pty`；IO 线程持有 terminal，渲染线程走两阶段 update（需要 unsafe Send 封装） | 能编译能跑；记录 `cargo build --release` 产物体积（stripped），分别测有无 `textlayout`、默认 feature 与最小 feature 三档 |
| 2 | 网格渲染两套实现：A 方案按行生成 TextBlob 并按 row id 缓存；B 方案自建 glyph atlas + `draw_atlas` | `cat` 大文件和 `yes` 刷屏时的吞吐（MB/s）；全屏 4K 下 1 行 dirty 和全屏 dirty 两种情况的帧时间 p50/p99；CPU 占用 |
| 3 | 1–3 个毛玻璃浮动 pane（`SaveLayerRec::backdrop` + blur sigma 10/20/40），底层持续刷屏 | 帧时间增量、GPU 时间（Instruments Metal System Trace）、MacBook 电池功耗；对比半分辨率下模糊再放大的方案 |
| 4 | TUI 验收：Claude Code、nvim（含 `:term`、鼠标、真彩、undercurl）、htop、lazygit、tmux 嵌套；测 2026 hold 加 1 s 超时、bracketed paste、OSC 52 读写、OSC 8 点击、OSC 9/777 通知、Kitty keyboard（nvim `:set keyprotocol`） | 一份逐项 pass/fail 清单；DA、DSR、XTVERSION 应答正确（可用 `vttest` 和 kitty `kitten` 的测试做辅助） |
| 5 | IME：macOS 自带拼音、日文 Kotoeri、韩文输入法 | preedit 绘制在光标处，候选窗位置正确；commit 后不重复输入；IME 开启时快捷键不串键 |
| 6 | （半天）Windows 冒烟测试：D3D12 后端，libghostty-vt 改用 DLL 或从源码 Zig 构建，ConPTY 跑 nvim | 能编译、能显示、能输入；把发现的链接问题和规避方法记下来 |

---

## 来源列表

**libghostty-vt / Ghostty**
- 博文：https://mitchellh.com/writing/libghostty-1-0
- 文档首页：https://lib.ghostty.org/vt/
- 头文件（vt-1.0.1）：https://github.com/ghostty-org/ghostty/tree/vt-1.0.1/include/ghostty/vt
  - 本文引用的有 terminal.h、render.h、modes.h、screen.h、grid_ref.h、unicode.h、device.h、key/encoder.h、key/event.h
- 指南（vt-1.0.1，`dist/doxygen/pages/`）：conventions.md、features.md、getting-started/installing.md，以及 guides/ 下的 effects.md、input.md、rendering.md、images.md、scrollback.md
- 打包和依赖：
  - https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/PACKAGING-LIBGHOSTTY.md
  - https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/dist/libghostty-vt/README-windows.md
  - https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/deps/libghostty-vt-third-party.json
- modes.zig：https://github.com/ghostty-org/ghostty/blob/vt-1.0.1/src/terminal/modes.zig
- Ghostty 字体和渲染（v1.4.0）：
  - https://github.com/ghostty-org/ghostty/tree/v1.4.0/src/font/shaper
  - https://github.com/ghostty-org/ghostty/blob/v1.4.0/src/font/Atlas.zig
- Ghostling：https://github.com/ghostty-org/ghostling

**Rust 绑定与生态**
- libghostty-rs：
  - 仓库：https://github.com/Uzaaft/libghostty-rs
  - build.rs：https://github.com/Uzaaft/libghostty-rs/blob/master/crates/libghostty-vt-sys/build.rs
  - crates.io 记录：https://crates.io/api/v1/crates/libghostty-vt
- awesome-libghostty：https://github.com/Uzaaft/awesome-libghostty

**Skia**
- rust-skia：
  - README：https://github.com/rust-skia/rust-skia/blob/0.157.0/README.md
  - skia-safe README：https://github.com/rust-skia/rust-skia/blob/0.157.0/skia-safe/README.md
  - Cargo.toml：https://github.com/rust-skia/rust-skia/blob/0.157.0/skia-safe/Cargo.toml
- rust-skia releases：
  - 0.157.0：https://github.com/rust-skia/rust-skia/releases/tag/0.157.0
  - 0.156.0：https://github.com/rust-skia/rust-skia/releases/tag/0.156.0
  - 0.153.2：https://github.com/rust-skia/rust-skia/releases/tag/0.153.2
- 预编译资产：https://github.com/rust-skia/skia-binaries/releases/tag/0.157.0
- skia-safe 源码（0.157.0，`skia-safe/src/`）：core/canvas.rs、effects/runtime_effect.rs、effects/image_filters.rs、core/font_mgr.rs、core/text_blob.rs
- Skia 上游：
  - SkCanvas.h：https://github.com/google/skia/blob/chrome/m157/include/core/SkCanvas.h
  - m157 RELEASE_NOTES：https://github.com/google/skia/blob/chrome/m157/RELEASE_NOTES.md
  - 后端目录：src/gpu/graphite、src/gpu/ganesh

**winit**
- releases：
  - v0.31.0-beta.4：https://github.com/rust-windowing/winit/releases/tag/v0.31.0-beta.4
  - v0.31.0-beta.3：https://github.com/rust-windowing/winit/releases/tag/v0.31.0-beta.3
- 源码（v0.31.0-beta.4）：
  - window.rs：https://github.com/rust-windowing/winit/blob/v0.31.0-beta.4/winit-core/src/window.rs
  - event.rs：https://github.com/rust-windowing/winit/blob/v0.31.0-beta.4/winit-core/src/event.rs
  - Wayland text_input：winit-wayland/src/seat/text_input/mod.rs
  - X11 IME：winit-x11/src/ime/mod.rs
- issues：#4807 #4777 #4524 #3095 #3814 #3617 #3092 #4525 #2569 #2780 #4508 #4412（https://github.com/rust-windowing/winit/issues/）

**先例**
- Neovide：
  - Cargo.toml：https://github.com/neovide/neovide/blob/main/Cargo.toml
  - rendered_layer.rs：https://github.com/neovide/neovide/blob/main/src/renderer/rendered_layer.rs
  - caching_shaper.rs：https://github.com/neovide/neovide/blob/main/src/renderer/fonts/caching_shaper.rs
- cmux：
  - 仓库：https://github.com/manaflow-ai/cmux
  - .gitmodules：https://github.com/manaflow-ai/cmux/blob/main/.gitmodules
- herdr：
  - 仓库：https://github.com/herdrdev/herdr
  - AGENTS.md、Cargo.toml
  - crates/ghostty-vt/build.rs
  - vendor/libghostty-vt.vendor.json、vendor/libghostty-vt.patches.md
  - src/protocol/render_ansi.rs
  - src/kitty_graphics/surface.rs
  - src/client/handshake.rs、src/client/frame_output.rs
  - docs/next/website/src/content/docs/ 下的 configuration.mdx、plugins.mdx、concepts.mdx
- Rio：
  - 仓库：https://github.com/raphamorim/rio
  - rio-window：https://github.com/raphamorim/rio/tree/main/rio-window
- Alacritty：https://github.com/alacritty/alacritty/blob/master/alacritty/Cargo.toml
