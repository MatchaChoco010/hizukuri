# color-management-v1 と color-representation-v1

- 確認した版: wayland-protocols 1.50（`3eab2dccf4bf`）。
  `wp_color_manager_v1` は version 3、`wp_color_representation_manager_v1` は version 1。
  Mesa 26.2.6、Linux kernel 7.4-rc2

XML を読む前に全体をつかむための文書である。細かい決まりは XML と、規範である `appendix.md` を確かめること。

## 1. 目的と全体像

**color-management-v1** は、色の意味をクライアントと compositor の間でやり取りするプロトコルである。

- クライアントは、表面の画素が「どういう表示装置と環境を想定して作られたか」を compositor に伝える。
- compositor は、出力が「どういう色の符号化を期待しているか」をクライアントに伝える。
- compositor は、表面ごとに「この形で出してくれると都合がよい」という希望も伝える。

これにより、compositor は、表面の内容を出力に合わせて変換できる。
sRGB の表面と HDR の動画を、1 つの画面に正しく並べられる。

**color-representation-v1** は、色の意味ではなく、バッファの中身の「表し方」を伝える。
alpha の掛け方、YCbCr から RGB への変換の係数、値の範囲、色差の標本の位置である。
得られた RGB がどの色を指すかは、color-management の側で決まる。2 つを組み合わせて使う。

中心になる概念は **image description** である。
これは「ある表示装置と、その視聴環境」を表す、作成後は変更できない object である。
表面にも、出力にも、同じ種類の object を使う。

## 2. 登場する object

| interface | 役割 |
| --- | --- |
| `wp_color_manager_v1` | global。compositor の能力を知らせる。ほかの object を作る入口 |
| `wp_color_management_output_v1` | `wl_output` ごとの拡張。出力の image description を得る |
| `wp_color_management_surface_v1` | `wl_surface` ごとの拡張。表面の image description を設定する。表面につき 1 つだけ |
| `wp_color_management_surface_feedback_v1` | `wl_surface` ごとの拡張。compositor が望む image description を得る。いくつ作ってもよい |
| `wp_image_description_creator_params_v1` | 数値の指定から image description を作る（parametric） |
| `wp_image_description_creator_icc_v1` | ICC プロファイルから image description を作る |
| `wp_image_description_v1` | image description。作成後は変わらない |
| `wp_image_description_info_v1` | image description の中身を、イベントの列として受け取る。`done` で自動的に消える |
| `wp_image_description_reference_v1` | image description への参照。ほかのプロトコルが image description を渡すためのもの。version 1 で固定 |

global に bind した直後の流れは次のとおりである。

1. compositor が、対応している rendering intent、feature、named の transfer function、named の primaries を、
   1 つずつイベントで送る（`supported_intent`、`supported_feature`、`supported_tf_named`、`supported_primaries_named`）。
2. 最後に `done` を送る。

## 3. image description の中身

parametric な image description は、次のものを持つ。

| 項目 | 意味 | 必須か |
| --- | --- | --- |
| transfer function | 電気的な値（符号値）と光の量の対応 | 必須 |
| primaries と white point | 色の符号化の基準になる色域（**primary color volume**） | 必須 |
| luminances | primary color volume の最小輝度、最大輝度と、**reference white** の輝度 | 省略できる（既定値がある） |
| mastering display primaries | 実際に使われる色域（**target color volume**）の原色 | 省略できる（primary と同じ） |
| mastering luminance | target color volume の最小輝度と最大輝度 | 省略できる（primary と同じ） |
| max_cll、max_fall | 内容の最大輝度、フレーム平均の最大輝度（CTA-861-H） | 省略できる（未定義のまま） |

**primary color volume と target color volume の違い。**
primary は「値をどう符号化するか」の基準で、target は「実際に表示されうる範囲」である。
BT.2020 で符号化した HDR の動画が、実際には P3 の範囲しか使っていない、という場合に、
primary を BT.2020、target を P3 と指定する。HDR の静的メタデータ（SMPTE ST 2086）に対応する。

target が primary より広い場合もある。scRGB がその例で、sRGB の原色で符号化しながら、
負の値や 1.0 を超える値で、sRGB の外の色を表す。これを受け付けるには、compositor が
`extended_target_volume` の feature を持っている必要がある。

**輝度の既定値。** transfer function によって決まる（XML の enum の説明による）。

| transfer function | 最小 | 最大 | reference white |
| --- | --- | --- | --- |
| 下のどれでもないもの（sRGB の既定） | 0.2 cd/m² | 80 cd/m² | 80 cd/m² |
| `bt1886` | 0.01 | 100 | 100 |
| `st2084_pq` | 0.005 | 10000 | 203 |
| `hlg` | 0.005 | 1000 | 203 |

- `st2084_pq` では、`set_luminances` の `max_lum` は無視される。最大は「最小 + 10000 cd/m²」になる。
- 最小輝度の引数は 10000 倍した整数である（小数 4 桁）。最大と reference white は、そのままの整数である。
- 色度の座標は 100 万倍した整数である（小数 6 桁）。

**色が未定義になる条件。** 次のどれかに当たる画素は、色が未定義になる。compositor がどう表示してもよい。

- transfer function の定義域の外の値
- target color volume を超える三刺激値
- `extended_target_volume` が無い場合は、primary color volume を超える三刺激値
- 浮動小数点の NaN と Inf

## 4. image description の作り方

image description を得る道は 6 つある。どれで作ったかで、`get_information`（中身の読み出し）ができるかが変わる。

| 作り方 | 必要な feature | `get_information` |
| --- | --- | --- |
| `create_parametric_creator` → `set_*` → `create` | `parametric` | できない |
| `create_icc_creator` → `set_icc_file` → `create` | `icc_v2_v4` | できない |
| `create_windows_scrgb` | `windows_scrgb` | できない |
| `create_windows_bt2100`（version 3 から） | `windows_bt2100` | できない |
| `wp_color_management_output_v1.get_image_description` | なし | できる |
| `wp_color_management_surface_feedback_v1.get_preferred`、`get_preferred_parametric` | なし（後者は `parametric`） | できる |

クライアントが自分で作ったものは、中身を自分で知っているので、読み出せない。
compositor が決めたもの（出力と、表面への希望）だけ、読み出せる。

### parametric

- creator に、項目を 1 つずつ設定する。同じ項目を 2 度設定すると、`already_set` のエラーになる。
- transfer function は `set_tf_named` か `set_tf_power` のどちらかで設定する。
  primaries は `set_primaries_named` か `set_primaries` のどちらかで設定する。
- named の値は、compositor が公開したものしか使えない。それ以外は `invalid_tf`、`invalid_primaries_named` になる。
- `set_tf_power` の指数は 1.0 以上 10.0 以下である。10000 倍した整数で渡す。
- `create` で、必須の項目がそろっているかを確かめる。足りなければ `incomplete_set` になる。
  `create` は creator を消す。
- 決まりに反してはいないが compositor が扱えない組み合わせは、エラーではなく、
  `wp_image_description_v1.failed`（原因は `unsupported`）で知らせる。

### ICC

- fd、offset、length で、ICC プロファイルを渡す。fd は seek と read ができなければならない（`bad_fd`）。
- 大きさは 32 MB までである。0 か、それを超えると `bad_size` になる。
- プロファイルは、version 2 か 4、3 チャンネル、class は Display か ColorSpace でなければならない。
  これに反する場合は、プロトコルエラーではなく、`failed`（`unsupported`）で知らせる。
- compositor は、fd の中身を書き換えてはならない。読むのは、`ready` か `failed` を送るまでである。

### Windows の scRGB と BT.2100

Windows のアプリ（Wine、Proton のゲーム）の内容をそのまま受け取るための、決め打ちの image description である。

- `create_windows_scrgb`: sRGB の原色、拡張した linear。値 1.0 が 80 cd/m²、最大の 125.0 が 10000 cd/m² に当たる。
  reference white は不明で、仮定が必要なら 2.5375（203 cd/m²）とする。target color volume も不明である。
- `create_windows_bt2100`: BT.2020 の原色、PQ。reference white は不明で、仮定が必要なら 203 cd/m² とする。
- 「不明」と書かれていることが要点である。Windows は、視聴環境に合わせた調整をせずに表示する。
  compositor も、ふつうの image description とは違う扱い（reference white を合わせない、など）をする余地がある。

### ready と failed

- `wp_image_description_v1` は、作った直後には使えない。`ready`（または `ready2`）が届いてから使う。
- 使えない状態の object に、破棄以外の要求を送ると `not_ready` のエラーになる。
  表面に設定しようとすると、表面の側の `image_description` のエラーになる。
- `failed` が届いた object は、破棄するしかない。原因は 4 つある。

| 原因 | 意味 |
| --- | --- |
| `low_version` | クライアントが bind した版では、その image description を伝えるイベントが足りない |
| `unsupported` | compositor が扱えない内容 |
| `operating_system` | クライアントに責任のない失敗（ファイルの読み込みなど） |
| `no_output` | 出力がすでに無い |

### identity

- `ready` と `ready2` は、image description の識別番号を運ぶ。
  同じ番号なら、compositor の中の同じ記録を指している。クライアントは、中身の読み出しを省ける。
- version 1 の `ready` は 32 bit で、番号が使い回されることがある。
- version 2 の `ready2` は 64 bit（上位と下位の 2 つの引数）で、**compositor は番号を使い回してはならない**。
- 0 は無効な番号である。

## 5. 表面、出力、feedback

### 表面（`wp_color_management_surface_v1`）

- `set_image_description` で、image description と rendering intent を設定する。
  **double-buffered** であり、`wl_surface.commit` で有効になる。
- 設定は「コピー」である。設定した直後に image description を破棄しても、表面の状態には影響しない。
- rendering intent は、compositor が公開したものしか使えない（`render_intent` のエラー）。
- **ready な image description は、compositor が必ず受け付けなければならない。**
- 何も設定していない表面の扱いは、compositor が決める。sRGB として扱うことが勧められている。
- `unset_image_description` と、object の破棄は、設定を外す。これも commit で有効になる。
- 1 つの `wl_surface` に、この object は 1 つしか作れない（`surface_exists`）。
- `wl_surface` が消えると、この object は inert になる。inert な object への要求は `inert` のエラーになる。

### 出力（`wp_color_management_output_v1`）

- `get_image_description` で、出力の今の image description を得る。
- 出力の image description が変わると、`image_description_changed` が届き、続いて `wl_output.done` が届く。
  クライアントは、もう 1 度 `get_image_description` を呼ぶ。
- 出力の global が消えると、この object は inert になる。そのとき `get_image_description` は `failed`（`no_output`）を返す。

### feedback（`wp_color_management_surface_feedback_v1`）

- compositor が、その表面について望む image description を知らせる。
- 望みが変わると、`preferred_changed`（version 1）または `preferred_changed2`（version 2 から）が届く。
  引数は、新しい image description の identity である。
- クライアントは `get_preferred` で中身を得る。parametric しか扱えないクライアントは `get_preferred_parametric` を使う。
- これは希望であって、強制ではない。クライアントは、どの image description を設定してもよい。
- 得た image description をそのまま表面に設定するのは、内容が完全に一致するときだけにする、という注意がある。
  target color volume がクライアントの必要より広いなら、クライアントは自分の値で作り直すべきである。

## 6. feature、primaries、transfer function、rendering intent

### feature

| feature | 意味 |
| --- | --- |
| `icc_v2_v4` | `create_icc_creator` が使える |
| `parametric` | `create_parametric_creator` が使える |
| `set_primaries` | 色度の座標で primaries を指定できる |
| `set_tf_power` | 指数で transfer function を指定できる |
| `set_luminances` | 輝度を指定できる |
| `set_mastering_display_primaries` | target color volume を指定できる。ただし primary の中に収まるものに限る |
| `extended_target_volume` | primary を超える target color volume も扱える。上の feature が前提 |
| `windows_scrgb` | `create_windows_scrgb` が使える |
| `windows_bt2100` | `create_windows_bt2100` が使える（version 3 から） |

### primaries（named）

`srgb`、`pal_m`、`pal`、`ntsc`、`generic_film`、`bt2020`、`cie1931_xyz`、`dci_p3`、`display_p3`、`adobe_rgb`。

### transfer function（named）

| 名前 | 内容 | `appendix.md` に式があるか |
| --- | --- | --- |
| `bt1886` | BT.1886。指数 2.4 で、黒の輝度を考慮する | ある |
| `gamma22` | 指数 2.2 の純粋なべき乗 | ある |
| `gamma28` | 指数 2.8 の純粋なべき乗 | ある |
| `ext_linear` | 線形。実数の全域で定義される | ある |
| `st2084_pq` | PQ（BT.2100） | ある |
| `compound_power_2_4`（version 2 から） | IEC 61966-2-1 の区分的な関数（sRGB の符号化の関数） | ある |
| `srgb`（version 2 で非推奨） | IEC 61966-2-1 sRGB | ない |
| `ext_srgb`（version 2 で非推奨） | IEC 61966-2-1 sYCC | ない |
| `hlg` | HLG（BT.2100）。最大 1000 cd/m² の表示装置を前提にする | ない |
| `st240`、`log_100`、`log_316`、`xvycc`、`st428` | そのほか | ない |

`appendix.md` は、値から輝度への対応を、次の形で定めている。
`O` は transfer function の結果、`L_W` と `L_B` は primary color volume の最大輝度と最小輝度である。

```text
L = (L_W - L_B) * O + L_B
```

PQ だけは、`L = 10000 cd/m² * O + L_B` である。

### rendering intent

`perceptual`、`relative`、`saturation`、`absolute`、`relative_bpc`、`absolute_no_adaptation`（version 2 から）。

- **compositor は `perceptual` に対応しなければならない。** ほかは任意である。
- ICC の rendering intent の考え方を、ICC でない image description にも当てはめる。

## 7. 版ごとの違い

| 版 | 入ったもの | wayland-protocols の版 |
| --- | --- | --- |
| 1 | 最初の版 | 1.41 で追加 |
| 2 | `ready2`、`preferred_changed2`（64 bit の identity）。`get_image_description`（参照から image description を得る）。`compound_power_2_4`。`absolute_no_adaptation`。`srgb` と `ext_srgb` の transfer function の非推奨。max_cll と max_fall が mastering luminance の範囲に入っていなければならない、という検査の廃止 | 1.47 と 1.48 が version 2 |
| 3 | `create_windows_bt2100` と、feature の `windows_bt2100` | 1.49 と 1.50 が version 3 |

### 「bind された版で非推奨のものは、公開してはならない」

`supported_intent`、`supported_feature`、`supported_tf_named`、`supported_primaries_named` の説明に、
次の決まりがある。

> A compositor must not advertise transfer functions that are deprecated in the bound version of the interface.

つまり、**クライアントが bind した版によって、公開する内容を変える**必要がある。

| クライアントが bind した版 | `srgb` | `compound_power_2_4` |
| --- | --- | --- |
| 1 | 公開してよい | 公開できない（version 2 で入ったため） |
| 2 以上 | **公開してはならない** | 公開してよい |

同じように、`ready` と `ready2`、`preferred_changed` と `preferred_changed2` も、bind された版で使い分ける。

### `srgb` の非推奨と `compound_power_2_4`

「sRGB」という名前が、2 つの違うものを指して使われてきた。

- IEC 61966-2-1 が定める、区分的な**符号化**の関数（暗い部分が直線、残りが指数 2.4 のべき乗）
- 実際の sRGB の表示装置の応答。ふつうは指数 2.2 の純粋なべき乗である

version 1 の `srgb` は、どちらを指すのかが曖昧だった。version 2 は、これを 2 つの名前に分けた。

- `gamma22`: 指数 2.2 の表示装置。XML は「動画、映画、コンピューターグラフィックスにはこれを使う」と勧めている。
- `compound_power_2_4`: 区分的な関数を、表示の側でも逆にたどる装置。
  XML は「ICC で較正した印刷のワークフローにはこれを使う」と勧めている。

**Mesa の Vulkan の WSI との関係。** Mesa 26.2.6 は、global を version 1 で bind する。
`VK_COLOR_SPACE_SRGB_NONLINEAR_KHR` と `VK_COLOR_SPACE_DISPLAY_P3_NONLINEAR_EXT` を、
`srgb` の transfer function に対応づけている。
compositor が、version 1 のクライアントに `srgb` を公開しない場合、次のようになる。

- sRGB の swapchain には、color-management の object を作らない。表面は「何も設定していない表面」になる。
- Display P3（非線形）の color space は、アプリから選べない。

## 8. compositor が守るべき決まり

実装するときに確かめる項目を並べる。

1. global を消してはならない（「should never remove」）。
2. bind の直後に、対応しているものをすべて送り、`done` を送る。bind された版で非推奨のものは送らない。
3. `perceptual` の rendering intent に対応する。
4. 公開していない feature の要求には、`unsupported_feature` のエラーを返す。
5. 公開していない named の値には、`invalid_tf`、`invalid_primaries_named` のエラーを返す。
6. ready な image description は、どれでも表面に設定できるようにする。
7. 表面の image description と rendering intent は、commit で有効にする。subsurface の同期にも従う。
8. `set_image_description` はコピーとして扱う。直後に image description が破棄されても、表面の状態を保つ。
9. `wl_surface` につき、`wp_color_management_surface_v1` は 1 つだけにする（`surface_exists`）。
10. 表面や出力が消えたら、拡張の object を inert にする。
11. 出力の image description が変わったら、`image_description_changed` と `wl_output.done` を送る。
12. 表面への希望が変わったら、`preferred_changed`（または `preferred_changed2`）を送る。
13. image description の identity は、0 にしない。version 2 以上では使い回さない。
14. `wp_image_description_info_v1` は、同じ image description については、いつも同じ内容を送る。
    parametric なら、`primaries`、`tf_named` か `tf_power`、`luminances`、`target_primaries`、`target_luminance` を必ず送る。
15. 輝度の検査をする。`max_lum` や `reference_lum` が `min_lum` 以下なら `invalid_luminance`。
    max_fall が max_cll より大きければ `invalid_luminance`。
    version 1 のクライアントに限り、max_cll と max_fall が mastering luminance の範囲に入っているかも検査する。
16. reference white を基準に、内容をそろえる（次の節）。

## 9. 合成の考え方

### reference white でそろえる

`set_luminances` の説明に、次の決まりがある。

> Compositors should make sure that all content is anchored, meaning that an input signal level of 'reference_lum' on one image description and another input signal level of 'reference_lum' on another image description should produce the same output level, even though the 'reference_lum' on both image representations can be different.

- **reference white は、「紙の白」「画面の中のふつうの白」の輝度である。**
- image description ごとに、reference white の絶対的な値は違う（sRGB の既定は 80、PQ の既定は 203 cd/m²）。
- compositor は、どの表面の reference white も、出力の上で同じ明るさになるように変換する。
- これにより、sRGB の白い文書と、PQ の動画の中の白い字幕が、同じ明るさに見える。
- HDR の内容の「reference white より明るい部分」（headroom）は、その比を保って、reference white より明るく出る。

絶対的な輝度（cd/m²）をそのまま再現するのではない、という点が要点である。
image description の輝度の値は、内容の中での相対的な関係を伝えるものとして使う。

### SDR と HDR の混在

- 出力が HDR のとき: SDR の表面の白を、出力の reference white（ユーザーが決める「SDR の明るさ」）に置く。
  HDR の表面は、それより明るい範囲を使える。
- 出力が SDR のとき: HDR の表面の reference white を、出力の白に合わせる。
  それより明るい部分は、出力の範囲に収まらないので、tone mapping で圧縮するか、切り捨てる。

### tone mapping と gamut mapping

- プロトコルは、方法を定めていない。rendering intent が、クライアントの希望を伝える。
- `perceptual` は「見た目がよくなるように、compositor が決めてよい」という意味になる。tone mapping をするなら、この intent のときである。
- それ以外の intent は、色を変えないことを求める方向である。範囲の外は切り捨てになる。

### 合成用の色空間

半透明の重ね合わせ（alpha blending）を、どの空間で計算するかである。プロトコルは、これを定めていない。

- 物理的に正しいのは、linear light での計算である。光の計算や、ぼかしなどの効果も、linear を前提にする。
- 既存のアプリは、符号化した値のまま重ねた見た目を前提に作られている。
  linear で重ねると、半透明の部分の見え方が、それらのアプリの想定と変わる。
- 表面の既定の alpha mode（`premultiplied_electrical`）は、符号化した値に alpha を掛けたものである（12 節）。
  linear で合成するには、いったん alpha で割り、transfer function を外し、もう 1 度 alpha を掛ける。

## 10. Vulkan の WSI と Mesa

Vulkan のクライアントは、ふつう、color-management の object を自分では作らない。Mesa の WSI が、swapchain の color space から作る。
ここに書くのは、Mesa 26.2.6 の `src/vulkan/wsi/wsi_common_wayland.c` の動作である。

- color-management と、`VK_EXT_hdr_metadata` の Wayland での対応は、Mesa 25.1.0 で入った。
- `wp_color_manager_v1` を **version 1** で bind する。
- compositor が公開した primaries と transfer function を集め、両方がそろう color space だけをアプリに見せる。

| Vulkan の color space | primaries | transfer function | 追加の条件 |
| --- | --- | --- | --- |
| `SRGB_NONLINEAR` | `srgb` | `srgb` | 常に見せる。compositor が両方を持つときだけ、表面に設定する |
| `DISPLAY_P3_NONLINEAR` | `display_p3` | `srgb` | |
| `EXTENDED_SRGB_LINEAR` | `srgb` | `ext_linear` | `extended_target_volume` が要る |
| `DISPLAY_P3_LINEAR` | `display_p3` | `ext_linear` | |
| `BT709_LINEAR` | `srgb` | `ext_linear` | |
| `BT709_NONLINEAR` | `srgb` | `bt1886` | |
| `BT2020_LINEAR` | `bt2020` | `ext_linear` | |
| `HDR10_ST2084` | `bt2020` | `st2084_pq` | |
| `HDR10_HLG` | `bt2020` | `hlg` | |
| `ADOBERGB_LINEAR` | `adobe_rgb` | `ext_linear` | |
| `PASS_THROUGH` | ― | ― | 常に見せる。Mesa は何も設定しない |

- swapchain を作るとき、color space が `PASS_THROUGH` でなければ、Mesa が `wp_color_management_surface_v1` を作る。
  parametric な image description を作って、表面に設定する。rendering intent は `perceptual` である。
- `VK_EXT_hdr_metadata` の値は、max_cll、max_fall、mastering display primaries、mastering luminance として渡す。
  mastering の値は、compositor が `set_mastering_display_primaries` を持つときだけ渡す。
- image description の作成が失敗したら、HDR のメタデータを外してやり直す。
- **アプリが自分で color-management の object を作りたいときは、`PASS_THROUGH` を選ぶ。**
  そうしないと、Mesa が作る object と衝突する（`surface_exists`）。

compositor の側から見ると、次のようになる。

- Vulkan のクライアントが HDR10 で出すには、`bt2020` と `st2084_pq` を公開する必要がある。
- scRGB で出すクライアントのためには、`ext_linear` と `extended_target_volume` が要る。
- version 1 で bind するクライアントのための経路（`srgb`、`ready`、`preferred_changed`）が要る。

## 11. KMS への伝え方

compositor は、合成の結果を、出力の image description に合わせて符号化する。
そのうえで、表示装置に「これから送る信号はこういうものだ」と伝える。Linux kernel（7.4-rc2）の、property の説明による。

| property | 付く object | 内容 |
| --- | --- | --- |
| `Colorspace` | connector | 送る信号の色の符号化（色域、YCbCr の変換の種類）を driver に伝える。transfer の特性は、`HDR_OUTPUT_METADATA` のほうが優先される |
| `HDR_OUTPUT_METADATA` | connector | HDR の静的メタデータ。driver が、HDMI の InfoFrame、DisplayPort の SDP として表示装置へ送る。EOTF、原色、最大と最小の輝度、max_cll、max_fall を含む |
| `max bpc` | connector | 1 チャンネルあたりのビット数の上限 |
| `VRR_ENABLED` | CRTC | 可変リフレッシュレートを使ってよい内容であることを伝える。connector の `vrr_capable` が真のときに効く |

- HDR で出すときは、`Colorspace` を BT.2020 の RGB にし、`HDR_OUTPUT_METADATA` で PQ を指定し、
  `max bpc` を 10 以上にし、10 bit 以上の framebuffer を使う。この 4 つを、1 つの atomic commit でそろえる。
- 表示装置の能力（対応する EOTF、原色、輝度）は、EDID から読む。
- kernel には、plane ごとに、合成の前にかける色の変換をハードウェアに任せる API（color pipeline。`drm_colorop`）がある。
  `Documentation/gpu/rfc/color_pipeline.rst` に説明がある。

## 12. color-representation-v1

- global の `wp_color_representation_manager_v1` に bind すると、compositor が対応を知らせる。
  `supported_alpha_mode` を alpha mode ごとに、`supported_coefficients_and_ranges` を係数と範囲の組ごとに送り、`done` で終える。
- `get_surface` で、表面ごとの `wp_color_representation_surface_v1` を作る。表面につき 1 つだけである（`surface_exists`）。
- 設定はすべて double-buffered で、commit で有効になる。

| 要求 | 内容 | 既定 |
| --- | --- | --- |
| `set_alpha_mode` | alpha の掛け方 | `premultiplied_electrical` |
| `set_coefficients_and_range` | YCbCr から RGB への係数と、値の範囲（full、limited） | compositor が決める |
| `set_chroma_location` | 色差の標本の位置（H.273 の `Chroma420SampleLocType`） | compositor が決める |

**alpha mode。**

| 値 | 意味 |
| --- | --- |
| `premultiplied_electrical` | 符号化した後の値に、alpha が掛けてある。Wayland の従来の扱いである |
| `premultiplied_optical` | 符号化する前の、光の量の値に、alpha が掛けてある |
| `straight` | alpha を掛けていない |

- linear light で合成する compositor にとって、`premultiplied_electrical` は扱いにくい。
  正しく linear に戻すには、いったん alpha で割り、transfer function を外し、もう 1 度 alpha を掛ける必要がある。
- `premultiplied_optical` と `straight` は、linear の合成にそのまま使える。

**係数。** `identity`（RGB の format 用）、`bt709`、`fcc`、`bt601`、`smpte240`、`bt2020`、`bt2020_cl`、`ictcp`。

**決まり。**

- 公開していない alpha mode、係数と範囲の組は、エラーにする（`alpha_mode`、`coefficients`）。
- commit のとき、バッファの pixel format と、係数や色差の位置が合っているかを検査する。合わなければ `pixel_format` のエラーにする。
  たとえば、RGB の format に YCbCr の係数を設定した場合である。
- 範囲の外の色差の位置の値は、`chroma_location` のエラーにする。
- 表面が消えたら、object を inert にする。
- 色差の位置の `type_0` から `type_3` は、XML に、Vulkan の `VkChromaLocation` との対応が書いてある。

## 13. 落とし穴

- **bind された版で、公開する内容とイベントを変える**ことを忘れやすい。
  試験では、version 1、2、3 のそれぞれで bind するクライアントを用意する。
- image description は、`ready` が届くまで使えない。compositor は、作成の要求に対して、
  必ず `ready` か `failed` のどちらかを送る。parametric は、すぐに `ready` を送れる。
  ICC は、解析に時間がかかるので、後から送ることになる。
- 表面の設定はコピーである。image description の object の寿命と、表面の状態の寿命を、結びつけてはいけない。
- 輝度の引数の単位が、項目によって違う（最小だけ 10000 倍）。
- PQ では、`max_lum` が無視される。
- `reference_lum` は、`max_lum` より大きくてもよい。その場合、reference white を出すには `extended_target_volume` が要る。
- `set_mastering_display_primaries` と `set_mastering_luminance` は、どちらも
  `set_mastering_display_primaries` の feature で使えるようになる。luminance のための別の feature は無い。
- target color volume が primary を超えていて、`extended_target_volume` が無い場合、結果は実装に任されている。
  勧められているのは、`failed` で断ることである。
- 申告が内容と違うクライアントがある（10000 cd/m² を超える輝度の申告など）。申告を、そのまま信用しない。
- クライアントは、Mesa のように global を version 1 で bind することがある。version 1 の経路を残す。

## 14. 出典

確認日はすべて 2026-11-12。

- wayland-protocols 1.50（`3eab2dccf4bfdd85c16d312a6c8c8fbc2a6e73a8`）:
  `staging/color-management/color-management-v1.xml`、`staging/color-management/appendix.md`、
  `staging/color-representation/color-representation-v1.xml`。1.47、1.48、1.49 の tag で interface の版を確認
  https://gitlab.freedesktop.org/wayland/wayland-protocols
- Mesa 26.2.6: `src/vulkan/wsi/wsi_common_wayland.c`、`docs/relnotes/25.1.0.rst`
  https://gitlab.freedesktop.org/mesa/mesa
- Linux kernel（7.4-rc2）: `drivers/gpu/drm/drm_connector.c` の property の説明、
  `Documentation/gpu/rfc/color_pipeline.rst`、`drivers/gpu/drm/drm_colorop.c`
  https://github.com/torvalds/linux
