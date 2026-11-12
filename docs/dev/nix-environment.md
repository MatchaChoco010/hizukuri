# Nix の devShell

リポジトリの直下の `flake.nix` と `flake.lock` が、開発環境を固定する。
Rust、ビルドに要るライブラリ、Mesa、validation layer、検証用のクライアントの版が、`flake.lock` で決まる。
CI は、この devShell の中で検査を走らせる（[テストと CI](testing-and-ci.md)）。
[検証用の VM](vm-environment.md)のゲストと QEMU も、同じ `flake.lock` から作る。

リポジトリを clone して、その中で、次を実行すると、CI と同じ検査が走る。

```sh
nix develop --command tools/ci.sh
```

要るのは、flake を有効にした Nix だけである。GPU も、画面も、要らない。
Nix は、必須ではない。ディストリビューションのパッケージだけでも、ビルドとテストができる（[開発環境](development-environment.md)）。
対応している system は、`x86_64-linux` だけである。

## 使い方

### devShell に入る

```sh
nix develop            # ふだん使う shell。エディター向けの道具と、試すためのアプリが入っている
nix develop .#ci       # CI が使う shell。検査に要るものだけ
nix develop .#slides   # リリースの報告のスライドを組む shell。Typst とフォントだけ
```

コマンドを 1 つだけ実行するときは、`--command` を使う。

```sh
nix develop --command cargo build
```

- 最初の 1 回は、パッケージを取得する。`/nix/store` に置かれる大きさは、`default` の shell で約 4 GiB、`ci` の shell で約 3 GiB である。
  2 回目からは、すぐに入る。
- shell の中の `cargo` と `rustc` は、Nix が用意したものである。版は、`rust-toolchain.toml` で決まる。
- **ビルドの結果は、`target/` の直下ではなく、`target/nix-<hash>/` に入る。**
  場所は、環境変数 `CARGO_TARGET_DIR` に入っている（[`target/` を分ける理由](#target-を分ける理由)）。

```sh
"$CARGO_TARGET_DIR/debug/proto-compositor" --backend headless --exit-after 60 ...
```

### 検査を走らせる

```sh
nix develop --command tools/ci.sh                               # CI と同じ検査
HIZUKURI_TEST_VALIDATION=1 nix develop --command tools/ci.sh    # validation layer つき
```

2 つの shell は、検査の条件が同じである。違うのは、入っている道具だけである。どちらで走らせても、CI と同じ結果になる。

### Vulkan のドライバーを選ぶ

devShell の中の Vulkan の loader には、nixpkgs の Mesa の lavapipe だけが見える。検査は、lavapipe で走る。
システムに入っているドライバーは、見えない。

- 手で起動するときに、lavapipe を明示するには、`--vulkan-icd` に manifest を渡す。場所は、環境変数に入っている。

```sh
"$CARGO_TARGET_DIR/debug/proto-compositor" --vulkan-device cpu --vulkan-icd "$HIZUKURI_ICD_LAVAPIPE" ...
```

- nixpkgs の Mesa の、ほかのドライバーも loader に見せるには、shell の中で、`VK_DRIVER_FILES` を、manifest のディレクトリに向ける。
  テストに使うデバイスは、`HIZUKURI_TEST_DEVICES` で選ぶ（`lavapipe`、`gpu`、`gpu,lavapipe`。既定は、`lavapipe`）。
  `gpu` で動くのは、Vulkan 1.4 と `VK_EXT_descriptor_heap` を持つドライバーである。

```sh
export VK_DRIVER_FILES="$HIZUKURI_ICD_DIRS"
vulkaninfo --summary
HIZUKURI_TEST_DEVICES=gpu,lavapipe tools/ci.sh
```

- devShell の中で使えるのは、nixpkgs の Mesa のドライバーだけである。システムに入っているドライバーは、
  別の glibc 向けにビルドされているので、devShell の中のプロセスには読み込めない。

### 気を付けること

- **devShell の中で、システムの Vulkan や GL のアプリを動かさない。** shell の環境変数は、nixpkgs の Mesa を指している。
  システムのバイナリは、別の glibc 向けの Mesa を読み込もうとして、失敗する。
  検証用のクライアントは、devShell に入っているものを使う。
- **devShell の中と外で、同じ `target/` のバイナリを使い回さない。** devShell の中では、`$CARGO_TARGET_DIR` を使う。
- devShell でビルドしたバイナリは、動的リンカーとライブラリを、`/nix/store` の絶対パスで指す。`/nix/store` の無い機械では、動かない。
  [検証用の VM](vm-environment.md)のゲストは、`/nix/store` を共有するので、devShell でビルドしたバイナリが、そのまま動く。
  ゲストへ送るバイナリは、devShell の中でビルドする。
- direnv は、必須にしていない。`.envrc` は、置いていない。

## devShell の中身

| | `default` | `ci` |
| --- | --- | --- |
| 用途 | ふだんの開発 | CI |
| Rust | `rust-toolchain.toml` の版と component に、`rust-src`、`rust-analyzer` を足したもの | `rust-toolchain.toml` の版と component |
| ビルドの道具 | `pkg-config`、gcc、`cargo-nextest`、Slang のコンパイラー（`shader-slang` 2026.21。`slangc`） | 同じ |
| ライブラリ | wayland、libxkbcommon、libinput、libseat、libudev、libdrm、libgbm、libdisplay-info 0.3、vulkan-loader | 同じ |
| Vulkan | Mesa、validation layer、`vulkaninfo`、`vkcube` | 同じ |
| テストが既定で使うアプリ | weston（`weston-terminal`）、wl-clipboard、foot | 同じ |
| そのほかのアプリ | alacritty、`wayland-info`、wev、Xwayland、xwayland-satellite、xeyes、xterm、fcitx5 と mozc、`drm_info` | なし |
| そのほか | python3（`tools/real-app-check.sh` が使う）、nixfmt、actionlint | なし |

QEMU は、devShell には入れていない。検証用の VM の道具（`tools/vm/vm`）が、`tools/vm/guest.nix` から、自分で用意する。

`slides` の shell は、上の 2 つとは別のものである。入っているのは、Typst と、スライドに使うフォント（Noto Sans CJK JP、JetBrains Mono）だけで、
Rust も、ライブラリも、入っていない。リリースの報告のスライド（skill `release-slides`）を組むときにだけ使う。
フォントの場所は、環境変数 `TYPST_FONT_PATHS` に入る。スライドを組む道具は、Typst に、機械に入っているフォントを探させない。
`default` と `ci` の shell は、Typst とフォントを取得しない。

## flake の出力

| 出力 | 内容 |
| --- | --- |
| `devShells.x86_64-linux.default`、`.ci` | 上の 2 つの shell |
| `devShells.x86_64-linux.slides` | リリースの報告のスライドを組む shell |
| `packages.x86_64-linux.vm-guest` | 検証用の VM のゲスト（NixOS の system）。`tools/vm/guest.nix` の `system` |
| `packages.x86_64-linux.vm-host-tools` | VM を動かす側の道具（QEMU、ssh、rsync、python）。`tools/vm/guest.nix` の `hostTools` |
| `formatter.x86_64-linux` | nixfmt |

- VM の 2 つは、`tools/vm/vm build` が `tools/vm/guest.nix` から直接ビルドするものと、同じ derivation である。
  flake の出力にしてあるのは、`nix flake check` が、ゲストの定義の評価まで確かめるようにするためである。
  `nix build .#vm-guest .#vm-host-tools` で、VM を起動せずに、ビルドだけを確かめられる。
- `tools/vm/guest.nix` は、`flake.lock` を自分で読み、同じ commit の nixpkgs を使う。

`flake.lock` は、nixpkgs の `nixos-unstable`（2026-11-07 の commit）を指している。Mesa は 26.2.6 で、lavapipe の LLVM は 21 である。
入っている版は、`nix develop --command vulkaninfo --summary` などで確かめる。

### 環境変数

| 変数 | 値 | 役割 |
| --- | --- | --- |
| `VK_DRIVER_FILES` | nixpkgs の Mesa の、lavapipe の manifest | loader が使うドライバーを、これだけにする |
| `VK_LAYER_PATH` | nixpkgs の validation layer | explicit layer を、nixpkgs のものだけにする |
| `VK_IMPLICIT_LAYER_PATH` | 空のディレクトリ | システムの implicit layer を読ませない |
| `HIZUKURI_SLANGC` | nixpkgs の `shader-slang` の `slangc` | build script が `.slang` を SPIR-V にするときに使う `slangc`（`prototype/slangc`）。`PATH` より先に見る |
| `HIZUKURI_ICD_LAVAPIPE` | lavapipe の manifest のパス | 手で lavapipe を選ぶときに使う |
| `HIZUKURI_ICD_DIRS` | nixpkgs の Mesa の `icd.d` | テストが、lavapipe の manifest を探す場所。システムの場所より先に見る。nixpkgs の Mesa の、すべてのドライバーの manifest がある |
| `CARGO_TARGET_DIR` | `<リポジトリ>/target/nix-<hash>` | システムのライブラリでビルドした結果と、分ける。すでに設定されていれば、変えない |
| `NIX_LDFLAGS` | `-rpath <vulkan-loader>/lib` など | `dlopen` で読むライブラリを、バイナリの RUNPATH に入れる |
| `GBM_BACKENDS_PATH`、`__EGL_VENDOR_LIBRARY_FILENAMES` | nixpkgs の Mesa | GBM と EGL が、nixpkgs の Mesa を見つける |
| `LP_NATIVE_VECTOR_WIDTH`、`LP_NUM_THREADS` | `128`、`4`。すでに設定されていれば、変えない | lavapipe の、CPU による結果の差を小さくする |

shell に入るときに、システムのファイルを読ませる次の変数を外す。

`LD_LIBRARY_PATH`、`VULKAN_SDK`、`VK_SDK_PATH`、`VK_ICD_FILENAMES`、`VK_ADD_DRIVER_FILES`、`VK_ADD_LAYER_PATH`、
`VK_ADD_IMPLICIT_LAYER_PATH`、`VK_INSTANCE_LAYERS`、`VK_LOADER_LAYERS_ENABLE`、`VK_LOADER_LAYERS_DISABLE`、`CMAKE_PREFIX_PATH`

## 作りと、その理由

### Rust の toolchain

rust-overlay の `fromRustupToolchain` で、`rust-toolchain.toml` を読む。版を決める場所が、このファイルの 1 つだけになる。
rustup を使う人も、同じファイルを読む。

- rust-overlay が配るのは、rustup が入れるものと同じ、公式のビルドである。
- profile の既定は、`minimal` である。`rust-toolchain.toml` に `profile` を書けば、そちらが使われる。
- devShell の中では、`PATH` の先頭に、Nix の toolchain が来る。`~/.cargo` の registry と git の依存は、devShell の外と共有される。

### Vulkan は、nixpkgs の lavapipe だけを見せる

システムの Mesa は、別の glibc と LLVM でビルドされている。devShell の中のプロセスには、読み込めない。
そこで、loader が探す場所を、すべて nixpkgs に向けている。
見せるドライバーを lavapipe だけにしているのは、どの機械でも、検査が同じデバイスで走るようにするためである。

- ドライバーは、`VK_DRIVER_FILES` で、nixpkgs の Mesa の、lavapipe の manifest だけにする。
- explicit layer（validation layer）は、`VK_LAYER_PATH` で、nixpkgs のものだけにする。
- implicit layer は、`VK_IMPLICIT_LAYER_PATH` を空のディレクトリに向けて、探させない。
  向けないと、loader は、`XDG_DATA_DIRS` とホームディレクトリの下から、システムの implicit layer の manifest を見つけて、
  システムの共有ライブラリを読み込もうとする。
- NixOS でない環境には `/run/opengl-driver` が無いので、GBM と EGL にも、Mesa の場所を教えている。

### `dlopen` で読むライブラリは、RUNPATH に入れる

vulkan-rs は、Vulkan の loader（`libvulkan.so.1`）を、実行時に `dlopen` で読み込む。リンクはしないので、場所を教える必要がある。
`LD_LIBRARY_PATH` は使わず、`NIX_LDFLAGS` に `-rpath` を足して、devShell の中でリンクしたバイナリの RUNPATH に入れている。

- `LD_LIBRARY_PATH` は、shell の中で動かす、すべてのプログラムに効く。RUNPATH は、そのバイナリにだけ効く。
- バイナリは、環境変数なしで、loader を見つける。
- RUNPATH に入れているのは、vulkan-loader、wayland、libxkbcommon である。
- `nix develop` は、既定で、RUNPATH に `<作業ディレクトリ>/outputs/out/lib` も足す。`NIX_NO_SELF_RPATH` で止めている。

### `target/` を分ける理由

cargo は、どのライブラリにリンクしたかを、記録していない。rustc の版が同じなら、ビルド済みとして扱う。
devShell の中と外で同じ `target/` を使うと、cargo は、システムのライブラリにリンクしたバイナリを、devShell の中でそのまま実行する。
そのバイナリは、nixpkgs の Mesa を読み込めず、Vulkan の初期化に失敗する。

- devShell に入るときに、`CARGO_TARGET_DIR` を、`target/nix-<hash>` にする。
- `<hash>` は、リンクするライブラリと、C のコンパイラーの store path から作る。
  `flake.lock` を更新すると、名前が変わり、最初からのビルドになる。古いライブラリにリンクしたバイナリを、使い続けない。
- `default` と `ci` の shell は、同じ `<hash>` になり、ビルドの結果を共有する。
- `.gitignore` の `/target/` が、そのまま効く。

### libdisplay-info は、0.3 の系列を入れる

nixpkgs の既定の `libdisplay-info` は、0.4 である。Rust の `libdisplay-info` crate は、0.4 より前を求める。
そのため、`libdisplay-info_0_3` を入れている。

### テストが、lavapipe の manifest を探す場所

テストは、lavapipe の manifest を、`find_icd_manifest`（`prototype/compositor/tests/common/icd.rs`）で探す。
この関数は、環境変数 `HIZUKURI_ICD_DIRS` を先に見て、その後に、システムの場所を見る。
devShell は、`HIZUKURI_ICD_DIRS` を、nixpkgs の Mesa の `icd.d` に向けている。
devShell の中のテストは、システムに Mesa があっても、nixpkgs の lavapipe を使う。

## 更新の手順

### `flake.lock` を更新する

`flake.lock` が、nixpkgs の commit を決めている。Mesa、LLVM、ライブラリ、クライアントの版は、すべて、ここで決まる。
**検証用の VM のゲストも、ここで決まる。** 更新すると、ゲストの kernel（nixpkgs の最新の系列である `linuxPackages_latest`）、
ゲストの Mesa、ゲストのアプリ、QEMU の版も変わる。
**自動では、更新しない。** Mesa の更新が要るときなどに、意図して更新する。ほかの作業のついでに、更新しない。

```sh
nix flake update                 # すべての入力を進める
nix flake update nixpkgs         # nixpkgs だけ
nix flake update rust-overlay    # rust-overlay だけ
git diff flake.lock              # 何が変わったかを見る
nix flake check                  # devShell と、VM のゲストの定義が、評価できること
nix develop --command tools/ci.sh
nix develop .#ci --command tools/vm/vm test tools/vm-check.py
```

- 更新のたびに、検査を通す。lavapipe の版が変わるので、画素の値を比べているテストの結果が変わらないことを確かめる。
  変わった場合は、原因を調べる。期待値を、黙って書き換えない。
- VM の中の試験も、通す。ゲストの kernel が変わるので、DRM/KMS の振る舞いが変わりうる。
  `tools/vm/reference/` と、KMS の模擬の入力にしている記録は、取ったときの kernel の版を、名前に持っている。
- 更新すると、`CARGO_TARGET_DIR` の名前が変わる。古い `target/nix-*` は、手で消す。
- この文書の「devShell の中身」の版の記述も、合わせて直す。

### Rust の版を上げる

`rust-toolchain.toml` を書き換える。`flake.nix` は、このファイルを読むので、書き換えなくてよい。
新しい版が、`flake.lock` の時点の rust-overlay に載っていなければ、評価が失敗する。
その場合は、`nix flake update rust-overlay` を、先に行う。

### そのほか

| すること | コマンド |
| --- | --- |
| `flake.nix` の整形 | `nix fmt flake.nix`（nixfmt） |
| flake の検査（devShell と、VM のゲストの定義の評価） | `nix flake check` |
| workflow の検査 | `nix develop --command actionlint` |
