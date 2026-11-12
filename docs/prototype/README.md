# 検証用の compositor

`prototype/` にあるのは、hizukuri の基盤の仕組みを確かめるための、検証用の compositor である。製品のコードではない。
この文書は、その作りと、動かし方、検査の回し方を説明する。

| 文書 | 内容 |
| --- | --- |
| この文書 | 構成、ビルド、起動時の指定、制御口、検査 |
| [KMS のバックエンド](kms-backend.md) | DRM/KMS の session としての作り、scanout の経路、安全策、自己診断、起動と終了、VM での検査 |
| [プロトコルの層](protocol-layer.md) | `proto-wl` が中核に見せる口、対応しているプロトコル、バッファの受け渡し |
| [スクリプトの層](script-layer.md) | スクリプトで描画とウィンドウ管理を書く仕組み、コアが見せる口、読み直し、故障したときの動き |

## 構成

| 場所 | 内容 | 主な依存 |
| --- | --- | --- |
| `prototype/color` | image description、原色の行列、transfer function、輝度の対応づけ。CPU の参照計算 | なし |
| `prototype/render` | Rust で書いた Vulkan のレンダラー。`wl_shm` の取り込み、linear light の `RGBA16F` での合成、出力への符号化、読み戻し。`--renderer rust` のときだけ使う | `vulkan-rs`、`naga`（ビルド時） |
| `prototype/layout` | Rust で書いたウィンドウの配置。単純なフローティング。`--wm rust` のときだけ使う | なし |
| `prototype/wl` | Wayland のサーバー側。Smithay の型を外へ出さない | Smithay |
| `prototype/backend` | バックエンドの trait、headless の実装、KMS の実装（模擬の DRM デバイスを含む） | `proto-render`、`calloop`、`drm`、Smithay（session、udev、libinput） |
| `prototype/gfx-core` | スクリプトに見せる、検査つきの GPU の API（Vulkan 1.4 と `VK_EXT_descriptor_heap`）。dmabuf の取り込み。SPIR-V で渡された shader の検証と検査 | `vulkan-rs`、`naga`、`spirv-tools`（SPIRV-Tools）、`rspirv` |
| `prototype/gfx-lua` | `proto-gfx-core` を Luau のスクリプトにつなぐ部分。時間とメモリの上限 | `mlua` |
| `prototype/compositor` | 中核（`compositor`）、描画の継ぎ目（`scene`）、ウィンドウ管理の継ぎ目（`wm`）、スクリプトの層（`script/`、`scripts/`）、誰も決めないときと、スクリプトが使えないときの、最小の描画とウィンドウの表（`fallback/`）、出力の画像（`targets`）、自己診断（`selftest`）、制御口（`control`）、実行バイナリ `proto-compositor` と `proto-ctl` | 上のすべて |
| `tools/test-client` | 検証用の Wayland クライアント。ライブラリと、バイナリ `proto-test-client` | `wayland-client` |
| `tools/ci.sh` | 検査をまとめて走らせる | |
| `tools/real-app-check.sh` | 実際のアプリ（weston-terminal）を headless の中で動かして、画面を取り込む | |
| `tools/vm-check.py` | 検証用 VM の中で、DRM/KMS の session としての動作をまとめて検査する | `tools/vm/vm` |

### 中核と、3 つの継ぎ目

中核（`proto_compositor::compositor::Compositor`）は、プロトコルの層から出来事を受け取り、入力を配り、フレームを描く時機を決め、
フレームの完了をクライアントに知らせる。描画、ウィンドウの配置、画面への出力は、自分では行わない。3 つの trait を通す。

| 継ぎ目 | trait | 実装 |
| --- | --- | --- |
| 描くもの | `proto_compositor::scene::Scene` | `ScriptScene`（スクリプトが描く）、`NullScene`（何も描かない）、`FlatScene`（`proto-render` で、ウィンドウを平らな矩形として描く。`--renderer rust` のときだけ） |
| ウィンドウを管理するもの | `proto_compositor::wm::WindowManager` | `ScriptWm`（スクリプトが決める）、`proto_compositor::fallback::Windows`（fallback の決まりで置く）、`proto_layout::Layout`（フローティング。`--wm rust` のときだけ） |
| 出力先 | `proto_backend::Backend` | `HeadlessBackend`、`KmsBackend` |

`Scene` の口は、次のとおりである。

- 内容を渡す口（`surface_buffer`）: 表面が commit したバッファを、image description、alpha mode と一緒に渡す。
- 出力へ描く口（`draw`）: 出力の描画先へ、ウィンドウの一覧とカーソルを描かせる。
- 当たり判定の口（`hit_test`、`locate`）: 画面上の点から、表面と、表面の中の座標を求めさせる。

描くものと、ウィンドウを管理するものは、起動時に選ぶ（`--renderer`、`--wm`、`--no-renderer`）。既定は、どちらもスクリプトである。
hizukuri が目指す形は、スクリプトの側である（[方針](../design/direction.md)）。Rust の実装（`proto-render` と `proto-layout`）は、
スクリプトの描画と数値を比べる相手として置いてあり、明示したときだけ使う。

| 指定 | 描くもの | ウィンドウを管理するもの | `state` の `window_manager` |
| --- | --- | --- | --- |
| 既定（`--renderer script --wm script`） | `ScriptScene` | `ScriptWm` | `script` |
| `--renderer script --wm rust` | `ScriptScene` | `proto_layout::Layout` | `rust-floating` |
| `--renderer rust --wm rust` | `FlatScene` | `proto_layout::Layout` | `rust-floating` |
| `--no-renderer` | `NullScene` | `fallback::Windows` | `fallback` |

- `--wm script` には、`--renderer script` が要る。`--renderer rust` のときは、`--wm rust` を付ける（付けなければ、起動に失敗する）。
- `ScriptWm` は、fallback のウィンドウの表（`fallback::Windows`）の上で動く。プロジェクトがウィンドウ管理の関数を定義しなければ、
  fallback の決まりで、ウィンドウを置き、focus を与える（[スクリプトの層](script-layer.md#fallback-のウィンドウ管理)）。
- `NullScene`（`proto_compositor::scene::NullScene`）は、ウィンドウの位置と不透明度を持ち、当たり判定には fallback の当たり判定で答える。
  何も描かず、表面の内容を持たない。GPU も、スクリプトも、使わない。

スクリプトの層の作りは、[スクリプトの層](script-layer.md)に書く。

### 色

- 合成は、linear light の `RGBA16F` のバッファで行う。作業用の色空間は、スクリプトが選ぶ（既定の描画）。
  Rust のレンダラー（`--renderer rust`）の作業用の原色は、BT.2020 である。
- 表面も出力も、image description（原色、transfer function、輝度）を持つ。色の指定が無い表面は、
  BT.709 の原色と、指数 2.2 のべき乗として扱う（`srgb-gamma22`）。
- 出力の既定も `srgb-gamma22` なので、既定の構成では、指定の無い表面の値が、そのまま出力に出る。
- `wp_color_management_v1` は、未実装である。表面の色は、制御口の `set_surface_color` で指定する。
- 8 bit の値は、合成用のバッファを往復すると、1 階調ずれることがある。0 に近い値は、数階調ずれる
  （半精度の浮動小数点が 0 の近くで粗く、出力の 2.2 の曲線がそれを引き伸ばすため）。

image description の名前:

| 名前 | 原色 | transfer function | 輝度（最小 / 最大 / reference white、cd/m²） |
| --- | --- | --- | --- |
| `srgb-gamma22` | BT.709 | 指数 2.2 のべき乗 | 0.2 / 80 / 80 |
| `srgb-piecewise` | BT.709 | IEC 61966-2-1 の区分的な関数 | 0.2 / 80 / 80 |
| `display-p3` | Display P3 | IEC 61966-2-1 の区分的な関数 | 0.2 / 80 / 80 |
| `adobe-rgb` | Adobe RGB (1998) | 指数 2.2 のべき乗 | 0.2 / 80 / 80 |
| `bt2020-gamma24` | BT.2020 | 指数 2.4 のべき乗 | 0.2 / 80 / 80 |
| `bt2020-pq` | BT.2020 | ST 2084 PQ | 0.005 / 10000 / 203 |
| `bt2020-linear` | BT.2020 | linear | 0.2 / 80 / 80 |
| `scrgb-linear` | BT.709 | linear（範囲の外の値を持てる） | 0.2 / 80 / 80 |

### バックエンド

- **headless**: 仮想の出力を、画面に出さずに描く。出力は複数作れる（左から右へ並ぶ）。仮想の frame clock が、
  リフレッシュレートの周期でフレームを許す。既定のバックエンドである。
- **KMS**: 仮想コンソールの上の DRM/KMS の session として動く。出力は 1 つだけ使う。
  明示したときだけ選ばれる（[KMS のバックエンド](kms-backend.md)）。
- nested（ほかの compositor の中のウィンドウとして動く）は、未実装である。

## ビルド

Rust の版は、`rust-toolchain.toml` で固定している。

```sh
cargo build -j 8
```

- KMS のバックエンドは、`libseat`、`libudev`、`libinput` を動的にリンクする。ビルドには、それぞれの開発用のパッケージが要る
  （Nix の devShell には入っている。[Nix の環境](../dev/nix-environment.md)）。
- Nix の devShell（`nix develop`）の中では、ビルドの結果が `target/` の直下ではなく、`$CARGO_TARGET_DIR`
  （`target/nix-<hash>/`）に入る。この文書の例の `target/debug/` は、`"$CARGO_TARGET_DIR/debug/"` に読み替える。
  devShell の中と外で、同じバイナリを使い回さない。
- シェーダーは WGSL で書き、ビルド時に `naga` で SPIR-V にする。外部のシェーダーコンパイラーは要らない。
- `proto-gfx-core` は、SPIRV-Tools を、crate `spirv-tools` が C++ の源からビルドして、静的にリンクする。
  ビルドに、C++ のコンパイラーが要る（devShell には入っている）。キャッシュの無いビルドが、その分、長くなる。

## headless での起動

Wayland や X11 の session の中の端末から起動するときは、その session につながらないように、専用の `XDG_RUNTIME_DIR` で起動する。

```sh
RT="$XDG_RUNTIME_DIR/hzk-dev"          # 短いパスにする（下の注意）
mkdir -m 700 "$RT"

env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET -u DISPLAY XDG_RUNTIME_DIR="$RT" \
    target/debug/proto-compositor --backend headless --socket hizukuri-dev --exit-after 60 &
SERVER=$!

# 検証用のクライアントは --socket が必須で、WAYLAND_DISPLAY を見ない
target/debug/proto-test-client --socket "$RT/hizukuri-dev" --color 800000ff --size 300x200 &

# 実際のアプリは、XDG_RUNTIME_DIR と WAYLAND_DISPLAY を明示する
env -u DISPLAY -u WAYLAND_SOCKET XDG_RUNTIME_DIR="$RT" WAYLAND_DISPLAY=hizukuri-dev weston-terminal &

# 制御口
target/debug/proto-ctl --socket "$RT/hizukuri-dev.control" state
target/debug/proto-ctl --socket "$RT/hizukuri-dev.control" screenshot artifacts/shot.png
target/debug/proto-ctl --socket "$RT/hizukuri-dev.control" quit

wait "$SERVER"          # 自分が起動したものが終わるのを待つ
rm -rf "$RT"
```

- **Unix socket のパスには、約 100 バイトの上限がある。** `XDG_RUNTIME_DIR` には、`/run/user/<uid>/` の下か、
  `/tmp` の下の、短い名前のディレクトリを使う。
- socket の名前は、自動では選ばない。既定は `hizukuri-0` である。同じ名前の socket が使われていれば、起動に失敗する。
  落ちた compositor が残した制御口の socket（誰も listen していないもの）は、消して起動する。
- SIGINT、SIGTERM、SIGHUP で、後始末をして終わる（出力を止め、socket を消す）。

### 起動時の指定

| 指定 | 既定 | 内容 |
| --- | --- | --- |
| `--backend` | `headless` | `headless`、`kms`、`auto`。KMS は、明示したときだけ選ばれる。`auto` は、`WAYLAND_DISPLAY` か `DISPLAY` があれば `headless`、無ければ `kms` |
| `--socket` | `hizukuri-0` | Wayland の socket の名前（`XDG_RUNTIME_DIR` の中）。`/` を含めば絶対パスとして扱う |
| `--control-socket` | `<socket>.control` | 制御口のパス |
| `--output-size` | `1280x720` | headless: 仮想の出力の大きさ（物理的な画素）。複数回指定すると、出力を左から右へ並べて作る（`HEADLESS-1`、`HEADLESS-2`、…） |
| `--output-scale` | `1` | 出力の scale。headless で出力が複数あるときは、1 つならすべてに、出力の数だけ指定すればそれぞれに使う |
| `--output-refresh` | `60` | headless: リフレッシュレート（Hz） |
| `--output-image-description` | `srgb-gamma22` | 出力の image description。上の表の名前で指定する |
| `--output-format` | `rgba8` | headless: 符号化後の画像の format。`rgba8`、`rgba16`、`bgra8`。KMS は、いつも `bgra8`（`XRGB8888`） |
| `--background` | `202020` | 背景色（`RRGGBB`）。BT.709 の原色 + gamma 2.2 の値として扱う |
| `--cursor` | `auto` | カーソルをフレームに描くか。`auto` は、KMS では描き、headless では描かない。`on`、`off` |
| `--vulkan-device` | `auto` | `auto`、`gpu`、`cpu`、またはデバイス名かドライバー名の一部（`llvmpipe` など）。KMS での `auto` は、画面を出している GPU |
| `--vulkan-icd` | なし | Vulkan のドライバーの manifest。`VK_DRIVER_FILES` に設定してから loader を読み込む |
| `--vulkan-validation` | off | validation layer を有効にする。layer が無ければ起動に失敗する |
| `--renderer` | `script` | 描くもの。`script`: スクリプトの層。`rust`: `proto-render`（比較用。`--wm rust` が要る） |
| `--wm` | `script` | ウィンドウを管理するもの。`script`: スクリプト（プロジェクトが決めないところは fallback）。`--renderer script` が要る。`rust`: `proto-layout` |
| `--no-renderer` | off | 何も描かずに動かす。GPU も、スクリプトも使わない。ウィンドウは、fallback が管理する。描画以外はすべて動く。`--renderer`、`--wm`、`--script` と一緒には指定できない |
| `--script` | 組み込み | スクリプトのプロジェクトのディレクトリ（`main.luau`）。無いモジュールは、組み込みのものを使う |
| `--script-on-error` | `fallback` | 描画のスクリプトが失敗したとき。`fallback`: 組み込みの fallback が描く。`hold`: 直前のフレームを出し続けて、後で試し直す |
| `--script-budget-ms` | `250` | スクリプトの 1 回の呼び出しに許す時間 |
| `--script-memory-mb` | `256` | スクリプトが使えるメモリ |
| `--script-retry-ms` | `1000` | `--script-on-error hold` のとき、試し直すまでの時間 |
| `--script-wait-gpu` | off | スクリプトの描画で、フレームごとに GPU の完了を待つ（計測用） |
| `--dmabuf` | `off` | クライアントから dmabuf を受け取るか（[プロトコルの層](protocol-layer.md#dmabuf)）。`off`、`cpu`、`gpu` |
| `--exit-after` | なし | 指定した秒数で終了する |
| `--kms-device` | seat の primary GPU | KMS: DRM のデバイス（`/dev/dri/card1` など） |
| `--kms-output` | つながっている最初の connector | KMS: 使う connector を 1 つ（`DP-1` など） |
| `--kms-mode` | 画面の preferred のモード | KMS: `幅x高さ` か `幅x高さ@Hz` |
| `--kms-scanout` | `auto` | KMS: `auto`、`dmabuf`、`cpu` |
| `--allow-real-hardware` | off | KMS: 仮想マシンではない機械で、画面を取ることを許す（[KMS のバックエンド](kms-backend.md#安全策)） |
| `--watchdog` | `15` | KMS: メインループがこの秒数止まったら abort する。`0` で無効 |

サブコマンド:

| コマンド | 内容 |
| --- | --- |
| `proto-compositor kms-info [--device PATH] [--anonymize] [--output FILE]` | DRM のデバイスの connector、mode、CRTC、plane、property を JSON で出す。読み取りだけで、DRM master を取らず、何も変えない |
| `proto-compositor kms-info --summary` | 人が読む一覧を出す。デバイスごとに、ドライバー、PCI の番地、seat の primary GPU かどうか、つながっている画面と preferred のモード |
| `proto-compositor kms-selftest [--device PATH] --output NAME --seconds N --report DIR` | 出力 1 つにテストパターンを出し、page flip を測り、報告を書いて終わる。パターンは、`proto-gfx-core` の上で直接描く（[KMS のバックエンド](kms-backend.md#デバイスの一覧と自己診断)） |

### Vulkan のデバイスの選び方

- スクリプトの描画と、`kms-selftest` のパターンの描画は、Vulkan 1.4 と `VK_EXT_descriptor_heap` が要る。
  Rust のレンダラー（`--renderer rust`）は、Vulkan 1.3（dynamic rendering と synchronization2）が要る。
- Vulkan の loader は、`VK_DRIVER_FILES` が設定されていると、そこに並んだドライバーだけを見る。
  設定されている環境で、別のドライバー（lavapipe など）を使うときは、manifest を明示する。

```sh
target/debug/proto-compositor --vulkan-device cpu --vulkan-icd "$HIZUKURI_ICD_LAVAPIPE" ...
```

- Nix の devShell の中では、lavapipe の manifest の場所が、環境変数 `HIZUKURI_ICD_LAVAPIPE` に入っている。
- 選ばれたデバイスは、起動時のログと、`state` の `renderer` に出る。`--no-renderer` では、`renderer` は `null` である。

## 制御口

検証のための口である。エディターとの通信とは別物である。

- Unix の stream socket。1 行に 1 つの JSON を送ると、1 行の JSON が返る。
- 成功すれば `{"ok": true, ...}`、失敗すれば `{"ok": false, "error": "..."}` が返る。
- キーとボタンの番号は、Linux の evdev の番号である（`KEY_A` は 30、`BTN_LEFT` は 272）。
- 座標は、全体の論理座標である。
- 制御口から注入した入力は、バックエンドのデバイスから来た入力と同じ経路を通る。

| `cmd` | 引数 | 内容 |
| --- | --- | --- |
| `ping` | | 応答を確かめる |
| `state` | | 状態を返す（下） |
| `screenshot` | `path`、`composite_path`（省略可）、`output`（省略可） | いまの状態を描いて、ファイルに書く。`path` は符号化後の画像（PNG）、`composite_path` は合成用の浮動小数点バッファ（OpenEXR）。`output` は出力の名前 |
| `key` | `keycode`、`pressed` | キーを押す、離す |
| `pointer_motion` | `x`、`y` | ポインターを絶対位置へ動かす |
| `pointer_motion_relative` | `dx`、`dy`、`dx_unaccel`、`dy_unaccel`（後の 2 つは省略可） | ポインターを相対的に動かす。出力の範囲に収める。`dx`、`dy` は加速を掛けた後の値、`*_unaccel` は掛ける前の値（省略すると、`dx`、`dy` と同じ）。どちらも、相対的な動きとしてクライアントに届く |
| `pointer_button` | `button`、`pressed` | ボタンを押す、離す |
| `pointer_axis` | `horizontal`、`vertical`、`v120_horizontal`、`v120_vertical`（どれも省略可） | スクロール。`v120_*` を付けると、ホイールとして送る |
| `move_window` | `window`、`x`、`y` | ウィンドウの論理的な位置を変える |
| `focus_window` | `window`（`null` で解除） | focus を移し、最前面に出す |
| `close_window` | `window` | ウィンドウに閉じるよう頼む |
| `set_window_opacity` | `window`、`opacity` | 描くときの不透明度 |
| `set_surface_color` | `surface`、`image_description`、`alpha_mode`（省略可） | 表面の値の意味を指定する |
| `script_call` | `function`、`args`（JSON の配列） | スクリプトの大域の関数を呼び、戻り値を `result` で返す |
| `script_reload` | `path`（省略可） | スクリプトを読み直す。`path` があれば、そのプロジェクトに替える。コンパイルできないモジュールがあれば、読み直さずに、失敗を返す（[スクリプトの層](script-layer.md#読み直し)） |
| `script_mode` | `mode` | `running`: スクリプトに任せる。`fallback`: 組み込みの fallback に任せる |
| `quit` | | 終了する |

`proto-ctl` は、これをコマンドにしたものである。失敗の応答のときは、終了コードが 0 以外になる。

```sh
proto-ctl --socket PATH state
proto-ctl --socket PATH screenshot out.png --composite out.exr
proto-ctl --socket PATH key 30 press
proto-ctl --socket PATH pointer-motion 100 80
proto-ctl --socket PATH pointer-motion-relative 10 0 --dx-unaccel 6 --dy-unaccel 0
proto-ctl --socket PATH pointer-button 272 press
proto-ctl --socket PATH pointer-axis --vertical 15 --v120-vertical 120
proto-ctl --socket PATH move-window 3 40 30
proto-ctl --socket PATH set-surface-color 2 display-p3
proto-ctl --socket PATH script-reload
proto-ctl --socket PATH raw '{"cmd":"state"}'
```

`state` が返すもの:

- `backend`、`session_active`、`socket`、`renderer`（デバイス名、ドライバー、validation のエラー数。`--no-renderer` では `null`）、`frames_rendered`
- `backend_status`: KMS のとき、ドライバー、connector、CRTC、plane、モード、scanout の経路、表示したフレーム数、
  flip の間隔の統計、拒否された commit の数。headless では `null`
- `frame_time_ms`: 最近のフレームの、描画と present にかかった時間の統計
- `composite_space`、`background`
- `outputs`: 大きさ、論理的な大きさ、位置、scale、format、image description、`encode_pipeline`
- `windows`（重なりの順。下から上）: 題名、app id、出力、位置、大きさ、`activated`、`visual`（見た目の位置と不透明度）、
  `surfaces`（役割、offset、大きさ、バッファの大きさ・種類・transform、`generation`、image description、alpha mode、`decode_pipeline`）
- `focus`: focus のあるウィンドウ、キーボードの focus のある表面、ポインターのある表面
- `pointer`: 位置、クライアントが求めたカーソル
- `selection`、`primary_selection`: クリップボードと primary selection の持ち主が出している MIME type
- `pointer_constraints`: クライアントが求めているポインターの制約。表面、ウィンドウ、種類（`lock`、`confine`）、
  有効かどうか（`active`）、cursor position hint
- `input_method`: 入力メソッドの有無（`bound`）、active か、キーボードを grab しているか、候補ウィンドウ（`popup`）の
  表面、見えているか、ウィンドウ、ウィンドウの中の位置、アプリが伝えたカーソルの矩形、IME に伝えた矩形
- `redraw_pending`: まだ描いていないフレームが、どれかの出力にあるか
- `window_manager`: `script`（`--wm script`）、`fallback`（`--no-renderer`）、`rust-floating`（`--wm rust`）
- `script`（`--renderer script` のとき）: モード、失敗の記録、読み直しの回数、呼び出しごとの時間、dmabuf の取り込みの統計

`decode_pipeline` と `encode_pipeline` は、どの変換をどの順でかけるかを表す文字列である。

```text
unpremultiply | eotf:gamma22 | primaries:srgb->bt2020 | luminance:x(80/80) | premultiply
luminance:x(80/80) | primaries:bt2020->display-p3 | clip[0,1] | inverse-eotf:srgb-piecewise
```

取り込んだ画像の形式:

| ファイル | 内容 |
| --- | --- |
| PNG | 出力の image description で符号化した値。8 bit か 16 bit の RGBA。色のメタデータは付けない |
| OpenEXR | 合成用のバッファそのもの。linear light、premultiplied alpha、reference white が 1.0。half float。圧縮なし。`chromaticities` に合成用の原色を書く |

## 検証用のクライアント

`proto-test-client` は、1 色で塗った `xdg_toplevel` を 1 つ出し、受け取ったイベントを 1 行 1 つの JSON で標準出力に書く。

```sh
proto-test-client --socket PATH [--title T] [--app-id ID] [--size WxH]
                  [--color RRGGBBAA] [--layout argb8888|xrgb8888|abgr8888|xbgr8888]
                  [--duration 秒] [--relative-pointer] [--lock-pointer | --confine-pointer]
                  [--text-input X,Y,W,H] [--input-method echo] [--no-window]
```

- `--socket` は必須である。`WAYLAND_DISPLAY` を見ない。
- `--relative-pointer` は、相対的な動きを求める。`--lock-pointer`、`--confine-pointer` は、ウィンドウに、
  persistent の lock か confine を求める（ゲームのように）。
- `--color` は、バッファに書く値そのものである。`wl_shm` は premultiplied なので、半透明の色は掛け算した値を渡す
  （50% の赤は `80000080`）。
- テストからは、ライブラリ（`proto_test_client::TestClient`）として使う。自分ではブロックせず、`pump` を呼んだときだけ進む。
  ライブラリは、バッファの模様、dmabuf での送付、buffer transform、viewport、fractional-scale、presentation の feedback、
  popup、selection、drag and drop、ウィンドウ管理への要求、相対的な動き、ポインターの制約、primary selection、
  data control も話す。ウィンドウを持たないクライアント（クリップボードの履歴の道具の役）にもなれる。
- `--text-input` は、ウィンドウに入力欄を持たせる。text-input に `enter` されると、その矩形をカーソルとして伝えて、
  有効にする。受け取った preedit と確定した文字列は、イベントとして出す。
- `--input-method echo` は、クライアントを、seat の入力メソッドにする（ふつうは `--no-window` と一緒に使う）。
  active な間、キーボードを grab し、英小文字を変換中の文字列にまとめ、Return で大文字にして確定し、
  候補ウィンドウの代わりに、1 色の popup を出す。ほかのキーは、仮想キーボードで返す。
- ライブラリは、入力欄の側（enable、カーソルの矩形、disable）と、入力メソッドの側（grab、preedit、commit、
  仮想キーボード、popup）を、1 つずつ操作できる（`ClientOptions::text_input`、`ClientOptions::input_method`）。

## 検査

検査の構成、回し方（`tools/ci.sh`）、共通の環境変数は、[テストと CI](../dev/testing-and-ci.md)にある。
ここには、検証用の compositor のテストに固有のことを書く。

テストは、画面を使わない。Vulkan 1.4 と `VK_EXT_descriptor_heap` を持つドライバーなら、どれでも動く。
既定では、lavapipe（CPU で描く Vulkan）で走る。CI も、lavapipe で回す。

| 環境変数 | 既定 | 内容 |
| --- | --- | --- |
| `HIZUKURI_TEST_DEVICES` | `lavapipe` | テストに使う Vulkan のデバイス（コンマで並べる）。`lavapipe` と、`gpu`（Vulkan の loader が見せるハードウェアのデバイス）。並べたデバイスが無ければ、失敗する |
| `HIZUKURI_TEST_REAL_CLIENTS` | `weston-terminal,wl-clipboard,foot,script-tiles` | 実際のアプリを使うテスト（`real_clients.rs`、`script_real_clients.rs`）で、使うもの。並べたものに要るプログラムが `PATH` に無ければ、失敗する。並べていないもののテストは、飛ばす |
| `HIZUKURI_TEST_SKIP_UDMABUF` | なし | `/dev/udmabuf` が要るテストを、外す。設定しなければ、`/dev/udmabuf` を開けないときに、それらのテストは失敗する（下） |
| `GFX_SPIKE_VALIDATION` | なし | `proto-gfx-core` と `proto-gfx-lua` のテストが読む、validation layer の指定。`tools/ci.sh` は、`HIZUKURI_TEST_VALIDATION=1` のときに、これも設定する |
| `HIZUKURI_LOG` | `info` | compositor のログの絞り込み（`tracing` の書式。`debug`、`proto_wl=trace` など）。テストが起動した compositor にも効く |

`HIZUKURI_TEST_REAL_CLIENTS` に並べられる名前:

| 名前 | 要るプログラム | 内容 | 条件 |
| --- | --- | --- | --- |
| `weston-terminal` | `weston-terminal` | toolkit のメニューが popup として開き、外を押すと閉じる | |
| `wl-clipboard` | `wl-copy`、`wl-paste` | selection の受け渡し。wl-clipboard は data control を使い、ウィンドウを出さない | |
| `foot` | `foot` | fractional-scale と viewport に従う端末 | |
| `alacritty` | `alacritty` | 同じことを、GL で描く端末で | 既定に入っていない |
| `script-tiles` | `weston-terminal` | スクリプトのウィンドウ管理が、端末をタイルに並べる | |
| `alacritty-dmabuf` | `alacritty` | GL のクライアントの dmabuf を、CPU で読む | 既定に入っていない。GPU の GL のドライバーが要る |
| `vkcube` | `vkcube` | Vulkan のクライアントの dmabuf を、スクリプトの描画のデバイスに取り込む | 既定に入っていない。`HIZUKURI_TEST_DEVICES` に `gpu` が要る |

- `proto-gfx-core` と `proto-gfx-lua` のテストは、プロセスの中で、1 回の実行につき 1 つのデバイスで走る。`HIZUKURI_TEST_DEVICES` に
  `gpu` が無ければ、lavapipe で走る。
- 終わらない shader の実験（`prototype/gfx-lua/tests/hazards.rs`）は、いつも、lavapipe の子プロセスで走る。
- **GPU なしで dma-buf を扱うテストは、kernel の `/dev/udmabuf` を使う。** テストを走らせる利用者が、このデバイスを開けることが要る。
  対象は、`script.rs` の dma-buf の取り込み、`kms_scanout.rs` の lavapipe での dma-buf の scanout、`proto-gfx-core` の `tests/dmabuf.rs` である
  （lavapipe は、dma-buf の書き出しと取り込みに、`/dev/udmabuf` を使う）。開けなければ、これらは失敗する。
  開けるようにできない環境では、`HIZUKURI_TEST_SKIP_UDMABUF=1` で外す。外したことは、`tools/ci.sh` の出力に出る。
- `prototype/compositor/tests/script_measure.rs` は、テストではなく、計測である（フレームの時間、当たり判定の費用、読み直しの時間）。
  既定では走らない。`cargo test --release -p proto-compositor --test script_measure -- --ignored --nocapture` で走らせる。
  結果は、`target/test-output/script-measure.md` に書く。
- DRM/KMS の session としての検査は、検証用 VM の中で、`tools/vm-check.py` が行う（[KMS のバックエンド](kms-backend.md#vm-での検査)）。

### テストが起動するプロセス

テストを走らせる機械では、テストで使うものと同じ名前のプロセスが、すでに動いていることがある
（クリップボードを見張る `wl-paste`、端末、別の `proto-compositor`）。テストと道具は、次の決まりで作ってある。

- **プロセスを、名前で探さない。名前で止めない。** 止めるのは、自分が起動したときに受け取った id と、
  そのプロセスに与えた process group だけである。
- **残ったプロセスの確認は、目印で行う。** `tools/ci.sh` は、実行ごとに違う値の環境変数 `HIZUKURI_TEST_RUN` を付けて
  テストを走らせる。テストが起動したプロセスと、その子は、これを引き継ぐ。終わった後に、この値を持つプロセスが
  残っていれば、id を表示して失敗する。止めはしない。
- **テストが起動するものは、テストを起動した session に届かない。** compositor にも、アプリにも、`WAYLAND_DISPLAY`、
  `WAYLAND_SOCKET`、`DISPLAY`、`XAUTHORITY`、`DBUS_SESSION_BUS_ADDRESS` を渡さない。値が、もとの `XDG_RUNTIME_DIR` の中を
  指している環境変数（compositor の制御用の socket など）も、どのプログラムのものであっても、渡さない。
  `XDG_RUNTIME_DIR` は、テストごとの専用のディレクトリである。アプリには、空の `HOME` と設定のディレクトリを渡す。
- 作業用のディレクトリ（`$XDG_RUNTIME_DIR/hzk-t-*`）は、テストが終わるときに消す。

### 実際のアプリを見て確かめる

```sh
tools/real-app-check.sh artifacts/real-app
tools/real-app-check.sh artifacts/real-app-lavapipe cpu "$HIZUKURI_ICD_LAVAPIPE"
```

weston-terminal を headless の `proto-compositor` の中で動かし、制御口からクリックとキーを送り、画面を PNG に取り込む。
合否は判定しない。`artifacts/` は git で管理しない。

## 未実装のもの

- nested のバックエンド。
- `wp_color_management_v1` と、それに続く色のプロトコル。HDR の出力、10 bit 以上の scanout。
- KMS での複数の出力、direct scanout、overlay plane、cursor plane、VRR、ティアリング。
- damage を使った部分的な描き直し（毎回、出力の全体を描く）。
- スクリプトから、SPIR-V の shader を渡すこと（`Gpu::create_shader_spirv` は、Rust の API にだけある）。
- Rust のレンダラーでの、dmabuf の Vulkan への取り込み（スクリプトの描画には、ある）。
- xwayland-satellite のための対応、タッチ、ジェスチャー。
- data control を、信頼するクライアントにだけ見せること（`wp_security_context_v1`）。
- 候補ウィンドウの位置を、スクリプトが決めること。text-input-v3 の version 2。
- Rust のレンダラーは、GPU の完了を CPU で待つ（表面の内容を取り込むたびと、フレームを描くたびに 1 回ずつ）。
  KMS でも、待ってから flip する（`IN_FENCE_FD` を使わない）。完了の通知が遅い環境では、
  更新のあるクライアントの数に比例して、フレームが遅くなる。
