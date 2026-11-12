# 開発環境

hizukuri をビルドし、テストし、動かすために要るものをまとめる。対応しているのは、x86_64 の Linux である。

開発の手順は、リポジトリの中のものだけで完結する。リポジトリを clone して、Nix の devShell に入り、検査を回す。CI も、同じことをする。

```sh
nix develop --command tools/ci.sh
```

GPU も、画面も、要らない。検査は、画面なし（headless）と、CPU で描く Vulkan（lavapipe）で動く。

環境の用意のしかたは、2 つある。どちらでも、ビルドとテストができる。

| 方法 | 内容 |
| --- | --- |
| Nix の devShell | `nix develop` で入る。Rust、ライブラリ、Mesa、検証用のクライアントの版が、`flake.lock` で固定される。CI も、これを使う（[Nix の devShell](nix-environment.md)） |
| ディストリビューションのパッケージ | この文書の表のものを、自分で入れる。版は、ディストリビューションで決まる |

ビルドの手順、起動時の指定、制御口は、[検証用の compositor](../prototype/README.md)にある。
検査の回し方は、[テストと CI](testing-and-ci.md)にある。

## Rust

- 版は、`rust-toolchain.toml` で固定している（1.96.0。`rustfmt` と `clippy` を含む）。
  rustup も、Nix の devShell も、このファイルを読む。版を決める場所は、ここだけである。
- 版を上げるときは、`rust-toolchain.toml` を書き換える。`Cargo.toml` の `rust-version` も合わせる。

## ビルドに要るライブラリ

Wayland のサーバー側（Smithay）と、KMS のバックエンドが使う。どれも、動的にリンクする。

| ライブラリ | 用途 | パッケージの名前の例（Fedora） |
| --- | --- | --- |
| C と C++ のコンパイラー、`pkg-config` | C と C++ の依存のビルドと、ライブラリの検出 | `gcc`、`gcc-c++`、`pkgconf-pkg-config` |
| Slang のコンパイラー（`slangc`） | `.slang` の shader を、ビルド時に SPIR-V にする（下） | `shader-slang`（nixpkgs の名前） |
| libwayland | Wayland のサーバーとクライアント | `wayland-devel` |
| libxkbcommon | keymap | `libxkbcommon-devel` |
| libinput | 入力（KMS） | `libinput-devel` |
| libseat | session（KMS） | `libseat-devel` |
| libudev | デバイスの検出と hotplug（KMS） | `systemd-devel` |
| libdrm、libgbm | DRM、scanout 用のバッファ（KMS） | `libdrm-devel`、`mesa-libgbm-devel` |
| libdisplay-info（0.3 の系列） | EDID の解析（KMS） | `libdisplay-info-devel` |
| Vulkan の loader | 実行時に `dlopen` で読み込む | `vulkan-loader` |

- **Slang のコンパイラー（`slangc`）が要る。** `proto-compositor` の build script（`build.rs`）が、
  `prototype/compositor/shaders/` の `.slang` を SPIR-V にして、バイナリに埋め込む。
  `slangc` は、環境変数 `HIZUKURI_SLANGC` が指すもの、無ければ `PATH` のものを使う（`prototype/slangc`）。
  devShell は、`flake.lock` が固定する nixpkgs の `shader-slang`（2026.21）を入れ、`HIZUKURI_SLANGC` を、それに向ける。
  devShell の外でビルドするなら、同じ版の `slangc` を用意する。見つからなければ、ビルドは、その旨を書いて失敗する。
- C++ のコンパイラーが要る。`proto-gfx-core` が使う SPIRV-Tools（crate `spirv-tools`）は、C++ の源からビルドされる。
- スクリプトの shader は、いまは、WGSL と GLSL で書き、実行時に、`proto-gfx-core` が `naga` で SPIR-V にする。
- スクリプト言語は、決まっていない。いまは、仮の言語として Luau を使っている。`mlua` が、Luau をソースからビルドする。
  システムのライブラリは、要らない。

## Vulkan

hizukuri は、Vulkan 1.4 と `VK_EXT_descriptor_heap` を持つドライバーなら、どれでも動く。
テストと CI は、lavapipe（CPU で描くドライバー）で回す。GPU は、要らない。

| もの | 用途 |
| --- | --- |
| lavapipe（Mesa 26.2 以降） | テストと CI が使うドライバー。Nix の devShell には、入っている |
| validation layer | Vulkan の使い方の誤りの検出。`HIZUKURI_TEST_VALIDATION=1` を付けたテストに要る |
| `vulkaninfo` | デバイスと拡張の確認。ドライバーが、Vulkan 1.4 と `VK_EXT_descriptor_heap` を持つかは、これで見る |

- 描くもの（スクリプトの描画と、`kms-selftest` のパターンの描画）は、どれもスクリプトに見せる GPU の API（`proto-gfx-core`）の上で動き、
  Vulkan 1.4 と `VK_EXT_descriptor_heap` が要る。
- **どのデバイスで描いたかを、いつも明示する。** デバイスの選び方は、3 つある。

| 手段 | 内容 |
| --- | --- |
| `--vulkan-device` | `proto-compositor` の起動時の指定。`auto`、`gpu`、`cpu`、またはデバイス名かドライバー名の一部 |
| `--vulkan-icd` | `proto-compositor` の起動時の指定。ドライバーの manifest を 1 つ指す。lavapipe に固定するときに使う |
| `VK_DRIVER_FILES` | Vulkan の loader の環境変数。指定した manifest だけを、loader に見せる。ほかのプログラム（`vulkaninfo` など）にも効く |

- テストに使うデバイスは、環境変数 `HIZUKURI_TEST_DEVICES` で選ぶ（[テストと CI](testing-and-ci.md)）。既定は、lavapipe である。
- GPU なしで dma-buf を扱うテストは、kernel の `/dev/udmabuf` を使う。テストを走らせる利用者が、このデバイスを開けることが要る
  （[テストと CI](testing-and-ci.md)）。
- テストは、lavapipe の manifest を、テストの共通の部品（`prototype/compositor/tests/common/icd.rs`）で、環境変数 `HIZUKURI_ICD_DIRS`（`:` で区切ったディレクトリの並び）、
  `/usr/share/vulkan/icd.d`、`/usr/local/share/vulkan/icd.d`、`/etc/vulkan/icd.d` の順に探す。
- 選ばれたデバイスは、`proto-compositor` の起動時のログと、制御口の `state` の `gpu` に出る。

## 検証に使うクライアント

自動のテストは、自分たちで書いた検証用のクライアント（`tools/test-client`）を使う。
内容の決まったバッファを出すので、フォントやテーマで結果が変わらない。

実際のアプリは、表示と操作を確かめるために使う。Nix の devShell には、次のものが入っている。
テストが既定で使うのは、`weston-terminal`、`wl-clipboard`、`foot` である。この 3 つは、`ci` の shell にも入っている。
ほかのアプリを使うテストは、環境変数で選んだときに走る。

| クライアント | 用途 |
| --- | --- |
| `weston-terminal`（weston） | `wl_shm` で描く端末。popup と grab の確認 |
| `wl-clipboard` | selection の確認 |
| `foot` | Wayland の機能を広く使う端末 |
| `alacritty` | GPU で描く端末。dmabuf の確認 |
| `vkcube`（vulkan-tools） | Vulkan の WSI を使うクライアント |
| `wev` | キーとポインターのイベントの表示 |
| `wayland-info`（wayland-utils） | 公開している global と、その版の一覧 |
| `xwayland-satellite`、Xwayland、`xeyes`、`xterm` | X11 のアプリの確認 |
| fcitx5、fcitx5-mozc | IME の確認 |
| `drm_info` | KMS の資源と property の一覧 |

テストが使う実際のアプリは、環境変数 `HIZUKURI_TEST_REAL_CLIENTS` で選ぶ（[検証用の compositor](../prototype/README.md)）。

## compositor を動かすときの決まり

開発に使う機械では、デスクトップの session が動いていることがある。その session と混ざらないように、次を守る。

- **専用の `XDG_RUNTIME_DIR` を使う。** 利用者の session の socket と、同じ場所に置かない。終わったら、自分で消す。
- **`WAYLAND_DISPLAY` と `DISPLAY` を外して起動する。** 付いたままだと、nested として、デスクトップにウィンドウを出してしまう。
- **クライアントは、compositor の socket を明示して起動する。**
- **自動終了の時間（`--exit-after`）を付ける。** プロセスを残さない。
- **止めるのは、自分が起動したプロセスだけである。PID で止める。** 名前の一致で止めない。
  デスクトップでは、テストで使うものと同じ名前のプロセス（端末、`wl-paste` など）が動いている。
- 画面を出さずに動かすときは、headless のバックエンドを使う。
- DRM/KMS の session としての動作は、[検証用の VM](vm-environment.md)の中で確かめる。
- X11 の display の番号は、デスクトップが使っているものと重ならないようにする。

Nix の devShell の中では、ビルドの結果が `target/` の直下ではなく、`$CARGO_TARGET_DIR` に入る（[Nix の devShell](nix-environment.md)）。
