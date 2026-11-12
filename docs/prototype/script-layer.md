# スクリプトの層

検証用の compositor は、描画と、ウィンドウ管理を、スクリプトで書ける。`--renderer script --wm script` で起動すると、
Rust のレンダラー（`hizukuri-render`）とレイアウト（`hizukuri-layout`）は使われず、スクリプトが、色の変換を含む描画、
当たり判定、ウィンドウの配置を受け持つ。

**スクリプト言語は、決まっていない。** いまは、検証のために Luau を使っている。言語に依存する部分は、
`crates/gfx-lua` と、`crates/hizukuri/src/script/` の中の値の受け渡しに限ってある。

## 形

```text
                    ┌──────────────── スクリプトの層（Luau。仮） ────────────────┐
                    │ main.luau → session.luau                                   │
                    │   render.luau  描画（2 pass）、当たり判定、変形             │
                    │   color.luau   伝達関数の WGSL        affine.luau  2D の変換 │
                    │   tile.luau    タイルの配置とキー操作                        │
                    └───────▲───────────────────────────────▲────────────────────┘
                     gfx.*（GPU）                      hz.*（compositor のコア）
                    ┌───────┴───────────────────────────────┴────────────────────┐
  crates/gfx-lua    │ ScriptHost: VM、時間の上限、メモリの上限                     │
  crates/gfx-core   │ Gpu: 検査つきの GPU の API、dmabuf の取り込みと書き出し       │
                    ├────────────────────────────────────────────────────────────┤
  crates/hizukuri   │ script/mod.rs    ScriptRuntime、ScriptScene（Scene の実装）  │
   src/script/      │ script/api.rs    hz の表                                     │
                    │ script/wm.rs     ScriptWm（WindowManager の実装）            │
                    │ script/dmabuf.rs クライアントの dmabuf をテクスチャにする     │
                    │ script/fallback.rs 組み込みの fallback（Rust で書いた描画）   │
                    ├────────────────────────────────────────────────────────────┤
  crates/hizukuri   │ Compositor: Scene と WindowManager の 2 つの継ぎ目の上で動く  │
                    │ （入力、focus の適用、描く時機、frame callback、制御口）       │
                    └───────┬───────────────────────────────┬────────────────────┘
              hizukuri-wl（プロトコル）            hizukuri-backend（headless、KMS）
```

- 描くもの（`Scene`）と、ウィンドウを管理するもの（`WindowManager`）の、スクリプトの側の実装が、`ScriptScene` と `ScriptWm` である。
- VM は 1 つである。描画とウィンドウ管理は、同じ VM の中の、同じスクリプトである。
- バックエンドは、誰が描くかを知らない。出力の画像を作る口（`TargetProvider`）を、Rust のレンダラーと
  スクリプトの側の両方が実装している。headless と KMS（CPU コピーと dma-buf）は、どちらにもつながる。
- スクリプトの描画は、Vulkan 1.4 と `VK_EXT_descriptor_heap` が要る。

## スクリプトの置き場所と、切り替え

- 組み込みのプロジェクトは、`crates/hizukuri/scripts/` にあり、バイナリに埋め込まれる。入口は `main.luau` である。
- `--script DIR` で、別のプロジェクト（`main.luau` のあるディレクトリ）を使う。プロジェクトに無いモジュールは、
  組み込みのものを使う。`hz.include("session")` の 1 行で、組み込みの動作から始めて、変えたい関数だけを置き換えられる。
- `--renderer script` で、描画をスクリプトに任せる。`--wm script` で、ウィンドウ管理も任せる（`--renderer script` が要る）。
  `--wm rust` のままなら、配置は `hizukuri-layout` が決め、スクリプトは描くだけである。

```sh
target/debug/hizukuri --backend headless --socket hz --renderer script --wm script --dmabuf gpu

# ウィンドウを傾ける（window の id と、2D の変換 a b c d tx ty）
hizukuri-ctl --socket "$RT/hz.control" script-call set_transform 3 '[0.94, -0.34, 0.34, 0.94, 80, 40]'
# 作業用の色空間
hizukuri-ctl --socket "$RT/hz.control" script-call set_working_space '"display-p3"'
# 読み直し、fallback
hizukuri-ctl --socket "$RT/hz.control" script-reload
hizukuri-ctl --socket "$RT/hz.control" script-mode safe
```

組み込みのプロジェクト:

- 描画（`render.luau`）: 作業用の色空間のバッファに合成してから、出力の符号の値に変換する、2 つの pass。
  作業用の色空間は、名前（`bt2020`、`srgb`、`display-p3`、`adobe-rgb`）か、色度の表で選ぶ。
- ウィンドウごとに、2D の変換（回転、拡大縮小、斜めの変形、移動）を持つ。当たり判定は、その逆をかける。
  popup と subsurface は、ウィンドウの表面の一覧に入っていて、同じ変換がかかる。
- タイル（`tile.luau`）: 最初の出力に、横に並べる。logo+L と logo+H で focus を左右に移し、shift を足すと隣と入れ替え、
  logo+Q で閉じる。キーの割り当ても、スクリプトの中にある。

## コアがスクリプトに見せる口

スクリプトから見えるのは、`gfx`（GPU）と `hz`（compositor のコア）の 2 つの表だけである。
スクリプトは、Wayland のオブジェクトにも、Vulkan のオブジェクトにも、直接は触れない。

### コアがスクリプトを呼ぶもの

| 関数（スクリプトが定義する） | いつ | 戻り値 | 定義しないとき、失敗したとき |
| --- | --- | --- | --- |
| `init(saved)` | 読み込んだ直後。`saved` は、前のスクリプトの `unload` の戻り値 | なし | 失敗すると、fallback に切り替わる |
| `unload()` | 読み直しの直前 | 引き継ぎたい値 | 引き継ぎなし |
| `frame(output, target)` | コアが、その出力のフレームを描くと決めたとき | なし | fallback が描く、または直前のフレームを保つ |
| `hit_test(x, y, accepts)` | ポインターの位置が変わるたび。フレームを描いた後にも | 表面の id と、その中の座標 | コアが、変形なしの位置で判定する |
| `locate(surface, x, y)` | ボタンを押している間の、ポインターの位置が変わるたび | その表面の中の座標 | 同上 |
| `window_opened(id)` | ウィンドウができた | なし | コアが、ずらして置き、focus を与える |
| `window_changed(id)` | ウィンドウの大きさが変わった | なし | 何もしない |
| `window_closed(id)` | ウィンドウが無くなった | なし | コアが、いちばん上のウィンドウに focus を移す |
| `focus_changed(id)` | スクリプト以外の理由で、focus が動いた | なし | 何もしない |
| `outputs_changed()` | 出力が増えた、減った、変わった | なし | 何もしない |
| `key(keycode, pressed, mods)` | キーが押された、離された | `true` なら、クライアントに渡さない | クライアントに渡す |
| `pointer_button(button, pressed, window, surface)` | ポインターのボタン | `true` なら、コアの「クリックで focus」をしない | クリックで focus |

- 毎フレーム必ず呼ばれる関数は、無い。アニメーションをするスクリプトは、`hz.request_frame()` で次のフレームを求める。
- `hit_test` の `accepts(surface, x, y)` は、その点が入力を受けるか（表面の input region）を答える、コアの関数である。
- ボタンを押している間は、押した表面にイベントを送り続ける。そのときの座標を、`locate` が答える。

### スクリプトがコアに尋ねるもの

| 関数 | 内容 |
| --- | --- |
| `hz.outputs()` | 出力の一覧。id、名前、画素の大きさ、倍率、論理座標での位置と大きさ、リフレッシュレート、画像の形式、色 |
| `hz.windows()` | ウィンドウの一覧（重なりの順）。id、title、app_id、位置、大きさ、focus、opacity |
| `hz.surfaces(window)` | ウィンドウの表面の一覧（subsurface と popup を含む。奥から手前） |
| `hz.damage(surface)` | 直前の更新で、クライアントが変わったと伝えた矩形（バッファの画素） |
| `hz.color_info(color)` | 表面の値の意味。image description の名前、原色、伝達関数、輝度、alpha の持ち方 |
| `hz.pointer()` | ポインターの位置 |
| `hz.cursor()` | 描くべきカーソル。組み込みの矢印のテクスチャ、またはクライアントの表面 |
| `hz.drag_icon()` | drag 中のアイコンの表面 |
| `hz.background()`、`hz.time()` | 背景色、起動からの秒数 |
| `hz.decode_transform(color, working)` | 表面の値を、作業用の原色の線形の値にする変換（行列、伝達関数の番号、alpha の扱い） |
| `hz.encode_transform(working, output)` | 作業用の値を、出力の符号の値にする変換 |
| `hz.decode_color(r, g, b, a, working)` | 1 色を CPU で変換する（背景色に使う） |

表面（`hz.surfaces` の 1 要素）:

| 項目 | 内容 |
| --- | --- |
| `id`、`role` | 表面の id、役割（`toplevel`、`subsurface`、`popup`） |
| `x`、`y`、`width`、`height` | ウィンドウの中での位置と、論理の大きさ |
| `texture`、`image` | テクスチャの slot と、画像の handle（`gfx` の呼び出しに渡す） |
| `buffer_width`、`buffer_height`、`scale` | バッファの大きさと、buffer scale |
| `uv` | 表面の矩形が、テクスチャのどこに当たるか（2D の変換）。viewport の切り出し、buffer transform、上下の反転は、ここに入っている |
| `color`、`alpha_mode`、`opaque` | 値の意味（番号。`hz.color_info` で中身を引ける）、alpha の持ち方、alpha が無いか |
| `generation`、`serial` | 画像を作り直した回数、内容が更新された回数 |
| `kind` | `shm`（画素を複写したもの）か `dmabuf`（取り込んだもの） |

色の変換の数値（原色の行列、伝達関数の番号と定数）は、コア（`hizukuri-color`）が出す。当てはめる shader は、スクリプトの側にある。

### スクリプトがコアに求めるもの

| 関数 | 内容 |
| --- | --- |
| `hz.request_frame()` | ほかに変化が無くても、次のフレームを描く |
| `hz.place(id, x, y)` | ウィンドウの位置 |
| `hz.configure(id, width, height)` | ウィンドウに求める大きさ（コアが configure を送る）。省くと、クライアントが決める |
| `hz.focus(id)` | キーボードの focus |
| `hz.raise(id)`、`hz.restack(ids)` | 重なりの順 |
| `hz.close(id)` | 閉じるように求める |
| `hz.include(name)` | プロジェクトのモジュールを読む |
| `hz.debug_composite(image, working)` | 検証用に、合成用のバッファを差し出す（画面の取り込みで EXR に書かれる） |
| `hz.log(text)` | ログ |

ウィンドウ管理の関数は、`--wm script` のときだけ使える（それ以外では、エラーが返る）。
存在しないウィンドウや、負の大きさも、エラーで返る。

### GPU の API（`gfx`）

`gfx` は、Vulkan の概念を見せつつ、すべての呼び出しを検査する API である（`crates/gfx-core`）。
誤った使い方は、プロセスを落とさずに、エラーの値で返る。

| | 内容 |
| --- | --- |
| スクリプトに見せるもの | buffer と image（用途を明示して作る）、shader、render pipeline の状態、compute pipeline、dynamic rendering の pass、viewport と scissor、vertex buffer と index buffer、draw、dispatch、push data、descriptor heap の slot |
| コアが受け持つもの | image の layout の遷移と barrier、command buffer、submit と fence、メモリの確保、破棄の遅延、descriptor heap の buffer と slot の割り当て、shader のコンパイルと reflection |

- スクリプトには、Vulkan のハンドルもポインタも渡さない。渡すのは、52 bit の整数の handle である
  （種類、device の世代、世代、番号）。倍精度の浮動小数点だけを持つ言語でも、正確に持てる幅である。
  破棄済み、種類の違い、作られたことのない値は、使ったときにエラーになる。
- `gfx.destroy` で handle は無効になるが、Vulkan の object と descriptor heap の slot は、それを使ったフレームを
  GPU が終えるまで残す。
- すべての image は、操作の外では、決まった layout にある（サンプリングできるものは `SHADER_READ_ONLY_OPTIMAL`、
  それ以外は `GENERAL`）。遷移は、コアが出す。
- shader は、`ShaderCompiler` という trait を通してコンパイルする。いまの実装は naga で、WGSL と GLSL を受け取る。
  配列と buffer の読み出しは範囲に切り詰め、device の `robustBufferAccess` と `robustImageAccess` を有効にする。
- 記録の呼び出しは、引数と状態を検査してから記録する。エラーを返した呼び出しは、何も記録していない。
- depth と stencil、MSAA、mip、テクスチャの配列と 3D、cube、indirect draw、query は、未実装である。

関数の一覧は、`crates/gfx-lua/src/api.rs` にある。

## コアに残っているもの

| コアが持つもの | 理由 |
| --- | --- |
| クライアントのバッファを、テクスチャにすること（`wl_shm` の upload、dmabuf の取り込み、クライアントへの release） | バッファの寿命は、プロトコルの決まりである。スクリプトが壊れても、テクスチャが残っていれば、fallback が描ける。スクリプトは、コアのテクスチャを壊せない（`gfx.destroy` がエラーを返す） |
| 出力の画像を作り、バックエンドに渡すこと | KMS の dma-buf の経路では、画像の作り方が、ディスプレイの側の制約で決まる。失敗したフレームを画面に出さないために、画像を 2 枚持つのもコアである |
| フレームを描く時機 | バックエンドの page flip と、session の状態に結び付いている。止まったスクリプトが、描く時機まで止めてはいけない |
| フレームの完了を、クライアントへ知らせること（frame callback、presentation feedback） | スクリプトが描けない間も、クライアントは、次のフレームを描いてよい時機を知らされる必要がある |
| configure を送ること、focus を適用すること | 何を求めるかは、スクリプトが決める。送る手順は、コアが受け持つ |
| ウィンドウの表（位置、大きさ、focus、重なりの順）と、その既定の値 | スクリプトが何も言わないとき、失敗したとき、fallback の間にも、新しいウィンドウに居場所と focus があるように |
| 入力の配り方と、緊急用のキー | ボタンを押している間の行き先、スクリプトが使ったキーの「離す」を渡さないこと。緊急用のキーは、スクリプトの `key` より先に、コアが見る |
| 色の変換の数値（`hizukuri-color`） | 原色の行列や、伝達関数の定数を、スクリプトごとに書き直さないため |
| 時間の上限、メモリの上限、失敗したときの扱い | スクリプトに任せられない |
| popup の位置（positioner の計算）、popup の grab、drag and drop | `hizukuri-wl` が持つ。スクリプトは、popup を、ウィンドウの表面の 1 つとして受け取り、描く |

- カーソルを描くのは、スクリプトである。どのカーソルを描くか（組み込みの矢印、クライアントの表面、隠す）は、
  コアが決めて、テクスチャとして渡す。
- 描くものが無ければ、フレームを描かない。

## dmabuf の取り込み

- `--dmabuf gpu` で、スクリプトの描画のデバイスが取り込める format と modifier を、クライアントに公開する
  （デバイスに尋ねて決める。`VK_EXT_image_drm_format_modifier`）。取り込むのは、1 plane の RGB（8 bit の 4 種と、16 bit float）である。
- 取り込んだバッファは、複写せずにテクスチャになる。スクリプトには、`wl_shm` と同じ「テクスチャ」として見える。
- クライアントが渡した fd が、本当に dmabuf かどうかを、コアが確かめる（`fstatfs`）。dmabuf でない fd は、
  デバイスに渡さず、mmap で読む。
- クライアントは、数枚のバッファを使い回す。取り込みは、表面ごとに 4 枚まで保ち、同じバッファを見つけて使い回す。
- クライアントのバッファは、使っている間、保持する。新しいバッファに替わったら、古いほうは、それを使ったかもしれない
  フレームを GPU が終えた後で、手放す。
- 取り込んだ画像を使い始めるときは、外の持ち主からの受け渡し（`VK_QUEUE_FAMILY_FOREIGN_EXT`）を記録する。
- 10 bit、YCbCr、複数の plane、明示的な同期（explicit sync）は、未実装である。

## 読み直し

スクリプトは、動かしたまま読み直せる（制御口の `script_reload`、または Ctrl+Alt+Shift+R）。

1. 新しいプロジェクトの `main` がコンパイルできるかを、先に確かめる。できなければ、動いているスクリプトを使い続ける。
2. 動いているスクリプトの `unload()` を呼び、戻り値を預かる。
3. スクリプトが作った GPU のオブジェクトを全部消し、VM の大域の状態を作り直す。
4. プロジェクトを読み、`init(saved)` を呼ぶ。

- コアのもの（クライアントのテクスチャ、出力の画像、ウィンドウの表）は、そのまま残る。ウィンドウは開いたままで、
  クライアントには、configure も focus の変化も届かない。
- 状態の引き継ぎは、スクリプトの仕事である。コアは、値を預かって渡すだけで、中身を知らない。
  組み込みのプロジェクトは、タイルの順番、作業用の色空間、ウィンドウの変換を引き継ぐ。
- スクリプトが外されている間に読み直すと、`unload` は呼ばれない（壊れたスクリプトの値を、信じない）。
- `script_reload` に `path` を付けると、別のプロジェクトに切り替わる。
- ファイルの変更を見て自動で読み直すことと、shader だけの差し替えは、未実装である。

## 故障したときの動き

スクリプトの呼び出しには、時間の上限（`--script-budget-ms`）と、メモリの上限（`--script-memory-mb`）がある。
スクリプトの故障で、compositor は落ちず、クライアントは切断されない。

時間の上限は、VM の interrupt（ループの折り返しと、関数の呼び出しと戻りのたびに呼ばれる）で見る。
interrupt は、`mlua` の `set_interrupt` を通さずに、VM に直接入れている（`crates/gfx-lua/src/interrupt.rs`）。
`mlua` の interrupt は、Rust の関数がエラーを返した後に、実行中のスクリプトの関数の stack を広げる。
失敗する呼び出しを `pcall` で受けながら繰り返すスクリプトでは、失敗の数に対して、時間が 2 乗で増える。

| 故障 | 検出のされ方 | その後 |
| --- | --- | --- |
| `frame` が例外を投げる、誤った GPU の呼び出しをする | エラーの値 | 下の「描画が失敗したとき」 |
| `frame` が、render pass を閉じずに戻る | コアが、戻った後に見つける | 同上 |
| `frame` が、メモリを使い続ける | VM のメモリの上限 | 同上 |
| `frame` が、戻ってこない | 時間の上限で、中断する | その 1 回だけ、上限の時間だけ止まる。以後は同上 |
| `window_opened` などの出来事の関数が、例外を投げる | エラーの値 | そのウィンドウは、コアの既定の場所に出て、focus を得る。スクリプトは続く |
| `hit_test` が例外を投げる | エラーの値 | その 1 回は、コアの判定（変形なし）を使う |
| `key` などが、戻ってこない | 時間の上限 | そのキーは、上限の時間の後で、クライアントに届く。スクリプトを、すぐに外す |
| 読み直したスクリプトが、コンパイルできない | 読み直しの前に検査する | 動いているスクリプトを、そのまま使い続ける |
| 読み直したスクリプトの `init` が失敗する | エラーの値 | fallback に切り替わる。直して読み直せば、戻る |

- **出来事に答える関数（当たり判定、キー、ウィンドウの出来事）には、回数の決まりがある。** 時間切れは 1 回で、
  ほかの失敗は 8 回続いたら、スクリプトを外す。ポインターの動きやキーのたびに呼ばれる関数が、毎回失敗する、
  または毎回、時間の上限まで走ると、セッションが使えなくなるためである。
- **スクリプトを外す**と、描画は fallback に、当たり判定とウィンドウ管理はコアの既定になる。ウィンドウは、その場所に残る。
  新しいウィンドウは、ずらして置かれて focus を得る。クリックで focus を移せて、閉じられる。

### 描画が失敗したとき

`--script-on-error` で選ぶ。

| 値 | 動き |
| --- | --- |
| `safe`（既定） | 組み込みの fallback（Rust で書いた小さな描画。スクリプトと同じデバイスの上で動く）に切り替える。ウィンドウは見えて、操作できる。変形と色の変換は無くなり、上の縁に、fallback が描いていることを示す帯が出る。戻すには、読み直すか、`script_mode running` |
| `hold` | 直前のフレームを出し続けて、`--script-retry-ms` ごとに試し直す。直れば、何もしなくても戻る。壊れたままだと、画面は止まったままになる |

- どちらでも、失敗したフレームは、画面に出ない。出力の画像を 2 枚持ち、スクリプトには、表示していないほうを渡す。
  成功したときだけ、入れ替える。
- 緊急用のキー: Ctrl+Alt+Shift+R で読み直し、Ctrl+Alt+Shift+F で fallback に切り替える。スクリプトより先に、コアが見る。
- Rust のレンダラー（`hizukuri-render`）への、実行中の切り替えは、無い（別の Vulkan のデバイスを持つため）。
- 制御口の `state` の `script` に、モード（`running`、`hold`、`safe`）、失敗の回数と直近の内容、読み直しの回数、
  呼び出しごとの時間、dmabuf の取り込みの統計が出る。

## 未実装のもの

- クライアントからの要求（最大化、全画面、最小化、移動、大きさの変更）を、スクリプトが受け取ること
  （`hizukuri-wl` は、出来事として出している。[プロトコルの層](protocol-layer.md#ウィンドウ)）。
- popup の位置を、変形に合わせて決めること（`hizukuri-wl` には、置き直す口がある。
  [プロトコルの層](protocol-layer.md#popup)）。いまは、変形していないウィンドウの位置で、出力の中に収める。
- damage を使って、描く範囲を狭めること。
- タイルが、複数の出力を使うこと。
- 画面に、エラーの内容を表示すること。
- クライアントの dmabuf について、commit を受けたところで、最大 100 ms、描き終わりを待つ処理が残っている
  （`hizukuri-wl` が、描き終わるまで commit を保留するので、この待ちは起きない）。
