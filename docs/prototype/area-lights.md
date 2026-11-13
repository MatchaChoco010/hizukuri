# 面光源

project `room`（`prototype/compositor/projects/room/`）は、モニターを面光源として、場面を照らす。モニターに映るウィンドウの内容が、
光源の色になる。その光の計算は、コアには無い。project の shader の module と、script の module と、数表で、できている。

手法は、LTC（Linearly Transformed Cosines）である: Heitz、Dupuy、Hill、Neubelt「Real-Time Polygonal-Light Shading with Linearly
Transformed Cosines」（SIGGRAPH 2016）。GGX の specular の lobe を、「cos の分布の向きを、行列で変換したもの」で近似する。
lobe が cos の分布になる空間では、多角形の光源の積分が、閉じた形で求まる。行列は、粗さと視線の角度ごとに、あらかじめ求めて、数表にしておく。

| もの | 場所 |
| --- | --- |
| Slang の module `ltc` | `prototype/compositor/projects/room/shaders/ltc.slang` |
| Luau の module `ltc`（数表の準備、光源の texture の mip） | `prototype/compositor/projects/room/ltc.luau`、`shaders/ltc_prefilter.slang` |
| 数表 | `prototype/compositor/projects/room/assets/ltc_mat.f32`、`ltc_amp.f32` |
| 数表を作る道具、CPU だけで確かめる道具 | `tools/ltc-fit`（crate `proto-ltc-fit`。バイナリ `ltc-fit`） |
| shader を動かして確かめる道具 | `tools/ltc-check`（crate `proto-ltc-check`。バイナリ `ltc-check`） |

module は、project `room` の中にある。ほかの project から `import` する口は、無い（[project の shader](script-layer.md#project-の-shader)）。

## 数表

2 枚とも、64×64 texel で、1 texel は `f32` が 4 つ（little endian。行ごと。ヘッダーなし）。`hz.load_image` の `raw` で、そのまま texture になる。
linear の補間で読む。texture の形式は、その device で linear の補間ができるものにする（Vulkan は、`rgba32float` の linear の補間を必須にしていない。
`hz.load_image` の `format = "rgba16float"` で変えられる。`room` は、`rgba16float` で読む）。

| ファイル | 引く位置 | 中身 |
| --- | --- | --- |
| `ltc_mat.f32` | (粗さ, √(1 − cos θ))。θ は、視線と法線の角度 | 当てはめた分布の、逆行列。真ん中の成分で割ってある。texel `(x, y, z, w)` が、行列 `((x, 0, y), (0, 1, 0), (z, 0, w))`（行ごと）を表す。座標は、z が法線、x が、視線を面に落とした向き |
| `ltc_amp.f32` の x、y | 同じ | BRDF × cos の積分（F = 1 のときの反射率）と、Schlick の (1 − v·h)⁵ で重みを付けた積分。specular に掛ける色は `f0 * (x − y) + y` |
| `ltc_amp.f32` の w | (向きの z × 0.5 + 0.5, 長さ) | その長さと向きの vector form factor を持つ、球の蓋の、地平線で切った form factor を、長さで割ったもの。多角形を地平線で切る計算の代わりに使う |

- `ltc_amp.f32` の z は、使わない（0）。
- 粗さは、見た目の粗さ（α は、その 2 乗）。格子は、粗さも √(1 − cos θ) も、0 から 1 までを等間隔に取る。
  列 0 は、粗さ 0.01 として当てはめてある。最後の行は、cos θ = 0.004 として当てはめてある。
- BRDF は、等方の GGX で、masking は、height-correlated の Smith である。Fresnel の項は、入っていない（上の 2 つの積分から、shader が戻す）。

### 作り直す

```sh
cargo run --release -p proto-ltc-fit -- fit prototype/compositor/projects/room/assets
```

- 格子の点ごとに、BRDF の積分と平均の向きを、可視の法線の分布の標本（512×512 の層別）で求める。
  lobe の軸の向き（BRDF の平均の向きからの回転）、2 つの幅、ずれの 4 つの数を、Nelder-Mead で探す（`--mean-axis` なら、軸は平均の向きのまま、3 つの数を探す）。
  最小にするのは、BRDF（積分で割ったもの）と分布の差の絶対値の、3 乗の積分である（`--power` で変えられる）。
  積分は、BRDF からの標本と、分布からの標本を、両方の密度の和で割って求める。
- 幅は対数で、ずれと軸の回転は、幅を単位にして探す。視線の角度ごとに、粗いほうから順に探し、隣の結果（と、それを α の比で細くしたもの）を出発点にする。
  角度の行は、並列に進む（`--threads`。既定は、CPU の数の半分）。乱数は使わないので、結果は、毎回同じである。
- `ltc-fit check <ディレクトリ>` は、数表を、BRDF と比べる（分布の差、積分、数表のなめらかさ）。四角形の光源を、総当たりの積分と比べる。
- `ltc-fit reads <ディレクトリ>` は、光源の上の、lobe の重みの平均の位置を数値で求め、shader が色を読む位置の候補と比べる。
  `ltc-fit shadows <ディレクトリ>` は、箱の影の近似を、箱を入れた総当たりの積分と比べる。

## Slang の module `ltc`

```slang
import ltc;

LtcTables tables = LtcTables(u_ltc_mat, u_ltc_amp, u_ltc_sampler);
LtcSurface s = ltc_surface(tables, position, normal, view, roughness);          // 点ごとに 1 回
LtcLight l = ltc_quad_textured(tables, s, LtcQuad(p0, p1, p2, p3, false),       // 光源ごとに
                               u_light, u_light_sampler, LTC_SIGMA_MIPMAPS);
color += albedo * l.diffuse + ltc_specular_colour(s, f0) * l.specular;
```

| 関数 | 内容 |
| --- | --- |
| `ltc_surface` | 照らされる点の準備（座標系と、数表の値）。数表を 2 回読む |
| `ltc_quad` | 四角形の光源の、diffuse の lobe と specular の lobe の取り分。lobe ごとに、form factor、光源の texture を読む位置、ぼかしの量を返す。地平線での切り取りは、数表（`ltc_amp.f32` の w）で求める |
| `ltc_quad_clipped` | 同じ。地平線での切り取りを、数表ではなく、多角形を切って求める。数表を読まないが、計算と分岐が多い |
| `ltc_light_colour` | lobe の、光源の色。光源の texture を、読む位置で、ぼかしの量に合う mip の段から読む |
| `ltc_quad_textured`、`ltc_quad_uniform` | 上をまとめたもの（テクスチャつきの光源、一様な色の光源） |
| `ltc_specular_colour` | specular に掛ける色（反射率の積分に、Schlick の Fresnel を入れたもの） |
| `ltc_box_cover`、`ltc_uncovered` | 箱の影の近似（下） |

- 位置と向きは、呼ぶ側が決めた 1 つの空間（world space など）で渡す。`view` は、面から目への向きである。
- 粗さが小さい（0.05 くらいより下）と、lobe が鋭すぎて、32 bit の計算と、光源の texture の段が足りない。呼ぶ側が、下を切る。
- 光源は、平行四辺形である。角は、texture の (0, 0)、(1, 0)、(1, 1)、(0, 1) の順に渡す。光るのは、その順が時計回りに見える側である
  （`LtcQuad` の `two_sided` が真なら、両側）。
- 光源の texture の値は、作業用の色空間の、linear の値である。結果も、その値のままである。
- **色を読む位置**は、光源の vector form factor の向き（光源の立体角の、平均の向き）を、lobe の軸のほうへ少し寄せた向きが、光源の平面と交わる点である。
  **ぼかしの量**は、その点までの距離に比例させる。mip の段のぼけ具合は、定数で、段の作り方に合わせて渡す
  （`gfx.generate_mipmaps` なら `LTC_SIGMA_MIPMAPS`、Luau の module の `update_light` なら `LTC_SIGMA_PREFILTER`）。
- 光源の texture の sampler は、段の間も linear で補間し、clamp する。光源の縁の外は、縁の色が伸びる。

### 箱の影

```slang
LtcLobes lobes = ltc_quad(tables, s, quad);
float4 cover = ltc_box_cover(s, quad, LtcBox(center, half_x, half_y, half_z));
lobes.diffuse.form_factor *= ltc_uncovered(lobes.diffuse, cover);
lobes.specular.form_factor *= ltc_uncovered(lobes.specular, cover);
```

- 物を、箱（中心と、直交する 3 つの、半分の辺のベクトル）で表す。箱が複数なら、箱ごとの `ltc_uncovered` を掛け合わせる。
- 照らされる点から見て、箱の 8 つの角が、光源の平面のどこに映るかを求め、それを囲む矩形を、隠される範囲とする。
  lobe の重みを、色を読む位置を中心にした Gaussian（広がりは、色を読むときのぼかしと同じ量）と見なして、矩形に入る分を引く。
- 数えるのは、箱のうち、照らされる面より上にある部分だけである。だから、凸な箱は、自分の表面に影を落とさない。
  箱の体積の中にある点は、その箱の影に入る。ほかの物の、照らされる面を、箱の中に含めない。
- 箱ごとに、まず、箱を囲む球で、影に入りえない点を除く。それでも、光源と箱の組ごとに、角を 8 つ映す計算がかかる。

## Luau の module `ltc`

| 関数 | 内容 |
| --- | --- |
| `ltc.create_tables({ mat, amp })` | 数表の texture（slot か、ファイルの中身）から、数表と sampler の組を作る。ファイルの中身なら、`rgba32float` の image にアップロードする（その形式の linear の補間ができる device に限る） |
| `ltc.bind_tables(tables, mat, amp, sampler)` | 数表を、shader の binding に bind する（pass の中で、`set_pipeline` の後） |
| `ltc.create_light(width, height, format)` | 光源の texture（すべての mip の段を持つ image と、作業用の image）。`format` の既定は `rgba16float` |
| `ltc.update_light(light)` | 段 0 から、下の段を作り直す。段ごとに、Gaussian でぼかしながら、半分にする（`shaders/ltc_prefilter.slang`）。pass の外で、段 0 が変わったときに呼ぶ |
| `ltc.destroy_light(light)` | 壊す |

- module は、`gfx` だけを使う。ウィンドウの内容を、光源の texture の段 0 に写すのは、呼ぶ側である（組み込みの module `color` の `blit_window` など）。
- 光源の texture の mip は、`gfx.generate_mipmaps` で作ってもよい（shader には `LTC_SIGMA_MIPMAPS` を渡す）。

## 確かめる道具

```sh
cargo run -p proto-ltc-check -- prototype/compositor/projects/room [--shots DIR] [--flicker] [--bench | --bench-only]
cargo test -p proto-ltc-check
```

- `ltc-check` は、compositor を起動しない。`proto-gfx-lua` の `ScriptHost` の上で、project の Luau の module を読み、自分の shader
  （`tools/ltc-check/shaders/check.slang`）から、project の `shaders/` の module を `import` して使う。shader は、その場で `slangc` でコンパイルする
  （`HIZUKURI_SLANGC`、無ければ `PATH`。devShell には入っている）。device は、テストと同じもの（`GpuConfig::for_tests`）である。
- 点ごとの値を、shader で求めて読み戻し、同じ計算の CPU 版（`proto-ltc-fit` の `shade`）と、総当たりの積分（`brute`）と比べる:
  一様な色の光源、絵を映した光源（mip の作り方 2 通り、形式 2 通り）、箱の影。`cargo test` は、shader と CPU 版の差と、総当たりとの差の上限を確かめる。
- `GFX_SPIKE_VALIDATION=1` のとき、validation layer の報告が 1 つでもあれば、失敗する。
- `--shots` は、床と箱を、四角形の光源で照らした場面を描いて、PNG を書く。`--flicker` は、光源の絵を 1 texel ずつ流して、照らされる点の値の変わり方を測る。
  `--bench` は、1920×1080 の 1 フレームの時間と、光源の texture の mip を作り直す時間を測る。`--bench-only` は、それだけを行う。

## 未実装のもの

- 箱で表せない物の影。物どうしの影の重なりの、正しい扱い。
- 平行四辺形でない光源（ほかの多角形、球、円盤、線）。
- GGX と Lambert 以外の BRDF（clear coat など）。
