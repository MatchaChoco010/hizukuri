# テストと CI

検査の構成と、回し方をまとめる。
テストごとの環境変数と、起動時の指定は、[検証用の compositor](../prototype/README.md)にある。

## 考え方

- **テストは、画面なし（headless）と、CPU で描く Vulkan（lavapipe）で動くものを、基本にする。**
  仮想コンソールも、ディスプレイも、GPU も、要らない。CI も、この範囲で回す。
  テストは、Vulkan 1.4 と `VK_EXT_descriptor_heap` を持つドライバーなら、どれでも動く。
- **ハードウェアに触る部分を、薄くする。** KMS のバックエンドは、DRM のデバイスに触る部分を、trait の向こうに置いている。
  判断のロジック（モードと plane の選び方、atomic commit の組み立て、フレームを描く時機）は、模擬のデバイスでテストする。
- **DRM/KMS の session としての動作は、[検証用の VM](vm-environment.md)の中で、自動で確かめる。**
- **色は、見た目ではなく、数値で確かめる。** 読み戻した画素を、CPU の参照計算と比べる。
- **回帰を防ぐ確認は、数値の検査にする。** 画像を見て確かめたものは、次に崩れたときに気づけない。
  見て確かめたものは、状態か画素の検査に直して、テストにする。
- **テストの合否を、機械の速さに依存させない。** 検査は、依存を最適化しないビルドで、遅い機械の上でも走る。
  - 戻ってくるはずの処理には、止まったときにだけ切れる、長い時間の上限を使う
    （スクリプトの呼び出し、GPU の完了待ち、出来事の待ち）。
  - 打ち切りを確かめるときは、打ち切られるはずの呼び出しにだけ、短い上限を付ける。
    かかった時間は、下限（上限より前に戻らないこと）で判定する。上側には、広い余裕を持たせる。
  - 「何も起きない」ことは、待つ時間ではなく、状態で判定する（描く予定のフレームが無いこと、関数が呼ばれた回数）。
  - 処理の量と時間の関係を確かめるときは、壁時計ではなく、スレッドの CPU 時間を比べる。

## 検査を回す

```sh
tools/ci.sh           # 全部
tools/ci.sh lint      # cargo fmt --check と、cargo clippy -- -D warnings
tools/ci.sh test      # cargo test --workspace と、プロセスが残っていないことの確認

nix develop --command tools/ci.sh           # devShell の中で。CI と同じ検査
```

変更を出す前に、`tools/ci.sh` を通す。CI が走らせるのも、同じスクリプトである。

| 環境変数 | 既定 | 内容 |
| --- | --- | --- |
| `HIZUKURI_TEST_DEVICES` | `lavapipe` | テストに使う Vulkan のデバイス（`lavapipe`、`gpu`。コンマで並べる）。並べたデバイスが無ければ、テストは失敗する（飛ばさない）。`gpu` は、loader に見えている、CPU でないデバイスを使う |
| `HIZUKURI_TEST_REAL_CLIENTS` | `weston-terminal,wl-clipboard,foot,script-tiles` | 実際のアプリを使うテストで、使うもの。並べたものに要るプログラムが `PATH` に無ければ、失敗する。並べていないもののテストは、飛ばす |
| `HIZUKURI_TEST_SKIP_UDMABUF` | なし | `1` にすると、`/dev/udmabuf` が要るテストを外す。設定しなければ、`/dev/udmabuf` を開けないときに、それらのテストは失敗する。外したことは、`tools/ci.sh` の出力に出る |
| `HIZUKURI_TEST_VALIDATION` | なし | `1` にすると、validation layer を有効にして走らせ、エラーが 0 であることも確かめる |
| `HIZUKURI_ICD_DIRS` | なし | Vulkan のドライバーの manifest を探すディレクトリ（`:` で区切る）。システムの場所より先に探す |
| `CARGO_BUILD_JOBS` | `8` | 並列にビルドする数 |

- Vulkan を使うテストは、`HIZUKURI_TEST_DEVICES` に並べたデバイスのそれぞれで走る。
  lavapipe では、テストが、`VK_DRIVER_FILES` を lavapipe の manifest に向けた子プロセスを起動する。環境の設定は、要らない。
- **GPU なしで dma-buf を扱うテストは、kernel の `/dev/udmabuf` を使う**（lavapipe が、dma-buf の書き出しと取り込みに使う）。
  テストを走らせる利用者が、このデバイスを開けることが要る。開けるようにできない環境では、`HIZUKURI_TEST_SKIP_UDMABUF=1` で外す。
  対象のテストと、`HIZUKURI_TEST_REAL_CLIENTS` に並べられる名前は、[検証用の compositor](../prototype/README.md)にある。
- 実行バイナリを使うテストは、テストごとに、専用の `XDG_RUNTIME_DIR` を作り、終わったら消す。
- 取り込んだ画像と、hizukuri のログは、`target/test-output/<テスト名>-<デバイス>/` に残る
  （devShell の中では、`$CARGO_TARGET_DIR/test-output/`）。
- **`tools/ci.sh` は、最後に、テストが起動したプロセスが残っていないことを確かめる。**
  実行ごとに違う値の環境変数（`HIZUKURI_TEST_RUN`）を付けてテストを走らせ、その値を引き継いだプロセスを探す。
  プロセスを名前では探さず、何も止めない。残っていれば、PID を表示して失敗する。

## テストの構成

| 種類 | 場所 | 内容 | Vulkan |
| --- | --- | --- | --- |
| 書式と lint | `tools/ci.sh lint` | `cargo fmt --check`、`cargo clippy` | 不要 |
| 色の計算 | `hizukuri-color` の単体テスト | 原色の行列と transfer function を、規格の値と比べる。往復で、元の値に戻ること | 不要 |
| 配置 | `hizukuri-layout` の単体テスト | 個別の場面と、proptest による不変条件 | 不要 |
| プロトコル | `crates/hizukuri/tests/protocol.rs`、`hizukuri-wl` の `tests/server.rs` | compositor と検証用のクライアントを、同じスレッドで動かす。レンダラーなし。configure、focus、キー、ポインター、バッファの解放、subsurface | 不要 |
| バックエンドの取り決め | `crates/hizukuri/tests/backend_contract.rs` | 中核を、台本どおりにイベントを出すバックエンドで動かす。出力の追加と削除、フレームの要求、session の一時停止と再開 | 不要 |
| KMS のロジック | `hizukuri-backend` の単体テスト、`tests/kms_mock.rs`、`tests/kms_fixtures.rs` | 模擬の DRM のデバイスで動かす（下の節） | 不要 |
| レンダラー | `crates/hizukuri-render/tests/gpu.rs` | 表面と出力の image description の組み合わせを描き、CPU の参照計算と比べる | 要る |
| KMS の scanout | `crates/hizukuri/tests/kms_scanout.rs` | 中核、KMS のバックエンド、模擬のデバイス、実際のレンダラーをつなぎ、模擬の「画面」の画素を確かめる | 要る |
| headless の全体 | `crates/hizukuri/tests/headless.rs` | 実行バイナリを headless で起動し、検証用のクライアントを出し、画面を取り込んで、画素を数値で確かめる。入力も確かめる。Rust のレンダラーと、スクリプトの描画の両方で走る | 要る |
| スクリプトの層 | `crates/hizukuri/tests/script.rs` | 2 つのレンダラーの画素の比較、作業用の色空間、dmabuf の取り込み、変形したウィンドウへの入力、タイル、スクリプトの故障と読み直し | 要る |
| スクリプトに見せる GPU の API | `crates/gfx-core/tests/`、`crates/gfx-lua/tests/` | 誤った呼び出しが、エラーの値になること。dmabuf の書き出しと取り込み | 要る |
| 実際のアプリ | `crates/hizukuri/tests/real_clients.rs`、`tests/script_real_clients.rs` | 実際のアプリを、headless の中で動かす。popup と grab、selection、fractional-scale、dmabuf | 要る |

検証用のクライアント（`tools/test-client`）は、内容の決まったバッファを出す。フォントやテーマで、結果が変わらない。
画素を比べるテストは、これを使う。

`tools/real-app-check.sh` は、テストではない。実際のアプリを headless の中で動かして、画面を PNG に書き出す。合否は、判定しない。

`tools/gpu-report.sh <ディレクトリ>` も、テストではない。その機械の GPU、kernel のドライバー、DRM のデバイスノードと、
Vulkan の loader に見えているドライバーの状態を、ファイルに書き出す。合否は、判定しない。読み取るだけで、何も読み込まず、何も変えない。

| ファイル | 内容 |
| --- | --- |
| `hardware.txt` | kernel の版、PCI の表示デバイス、`/dev/dri/` のノード、card ごとのドライバーと connector、読み込まれている GPU の module |
| `vulkaninfo.txt` | `vulkaninfo` の出力の全体 |
| `vulkan-summary.md` | デバイスごとの名前、API の版、ドライバーの版と、hizukuri が使う拡張（`VK_EXT_descriptor_heap` と、dma-buf の拡張）を公開しているデバイスの数 |

Vulkan の部分は、`PATH` の `vulkaninfo` が、その時の環境で見るものを書く。ドライバーを 1 つに絞るときは、`VK_DRIVER_FILES` を、その manifest に向けてから呼ぶ。

### 色の検証

- `hizukuri-color` は、純粋な計算である。原色から求めた行列と、transfer function を、規格の値と比べる。
  CPU の参照計算（デコード、合成、エンコード、量子化）も、ここにある。
- レンダラーのテストは、表面の image description と、出力の image description の組み合わせを変えて、何通りも描く。
  読み戻すのは、合成用の浮動小数点のバッファ（linear light）と、出力の符号化後の画像の 2 つである。
  - 浮動小数点のバッファは、参照計算との差（絶対誤差と相対誤差）で判定する。
  - 符号化後の画像は、参照計算を量子化した値との差が、符号値で決まった範囲に入ることで判定する。
- headless の全体のテストは、検証用のクライアントが color-management で image description を設定し、単色のバッファを出す。
  プロトコルで受け取った情報が、レンダラーまで届いていることを、出力の画素で確かめる。

### KMS のロジックのテスト

KMS のバックエンドは、DRM のデバイスに触る部分を、trait の向こうに置いている。
テストでは、その向こうを、kernel の決まり（atomic commit の検査など）を再現する、模擬のデバイスに差し替える。

- 模擬のデバイスは、実際のデバイスの記録から作る。記録は、`crates/hizukuri-backend/tests/fixtures/` にある
  （amdgpu、virtio-gpu、vkms のドライバーのもの）。`drm_info -j` の記録（`tools/vm/reference/`）も、読める。
- 記録は、次のコマンドで取る。デバイスを読み取り専用で開き、DRM master を取らず、何も変えない。
  `--anonymize` は、EDID から、モニターを特定できる情報（製造者、製品コード、製造番号、製造の時期、名前）を消す。
  リポジトリに入れる記録は、必ず `--anonymize` を付けて取る。

```sh
hizukuri kms-info --device /dev/dri/cardN --anonymize
```

- 模擬が再現するのは、kernel の文書に書かれた決まりと、実際のドライバーが commit を拒否した例である。
  実際のドライバーで、模擬と違う振る舞いが見つかったら、その例を、模擬とテストに足す。

## VM の中の試験

DRM/KMS の session としての動作（libseat、atomic commit、page flip、libinput、VT の切り替え、hotplug、watchdog）は、
検証用の VM の中で、`tools/vm-check.py` が確かめる。

```sh
nix develop .#ci --command tools/vm/vm test tools/vm-check.py
```

- 要るのは、Linux、KVM（`/dev/kvm`）、Nix である。GPU は、要らない。ゲストは、lavapipe で描く。
- ゲストは、`flake.lock` の nixpkgs で作る NixOS である（`tools/vm/guest.nix`）。kernel、Mesa、アプリ、QEMU の版は、`flake.lock` で決まる。
  ディスクは無く、起動するたびに、同じ状態から始まる。`/nix/store` を共有するので、devShell でビルドしたバイナリが、ゲストでそのまま動く。
- `vm test` は、ゲストをビルドし、VM を起動し、試験を走らせ、VM を止める。
- 既定で回るのは、GPU の要らない部である（`lavapipe`、`script`、`vkms`、`safety`、`outputs`）。
  VM の起動のしかたが違う部（`real-hardware`、`venus`）は、`--only` で、別に回す。構成が合わない VM に頼むと、失敗になる。

VM の作りと、試験の部の一覧は、[検証用の VM](vm-environment.md)にある。
この試験は、`ci.yml` ではなく、`vm.yml` が回す（下の「CI」）。

## 検査が届かない範囲

次のものは、模擬のデバイスでも、VM でも、確かめられない。自動の検査の範囲に、入っていない。

| 確かめられないこと | 模擬と VM で足りない理由 |
| --- | --- |
| HDR の出力（10 bit、`Colorspace`、`HDR_OUTPUT_METADATA`） | virtio-gpu は 8 bit で、HDR の property を持たない。vkms には、表示装置が無い。模擬は、commit の中身までしか確かめられない |
| VRR | virtio-gpu にも vkms にも、無い |
| ティアリング（async page flip） | virtio-gpu には、無い。async の commit を拒否する条件は、ドライバーごとに違う |
| direct scanout | どの format と modifier のバッファを、どの plane に載せられるかは、ドライバーと GPU ごとに違う |
| GPU のドライバーでの、Vulkan から KMS への経路 | VM の既定の構成は、GPU を使わない。Venus の構成は、仮想化を通る |
| page flip の実際の時刻と、遅延 | VM の時刻は、外側の機械の都合で揺れる |
| 複数の GPU | VM には、GPU が 1 つしか無い |
| 画面に出た光（HDR の見え方、ティアリングの見え方、色） | 取り込む手段が無い |

## lavapipe の結果の再現性

lavapipe（llvmpipe）は、実行時に、その CPU 向けのコードを作る。CPU の命令セットが違う機械では、結果が少し違うことがある。

- Mesa と LLVM の版は、`flake.lock` で固定している。
- devShell は、`LP_NATIVE_VECTOR_WIDTH=128` と `LP_NUM_THREADS=4` を設定している。ベクトルの幅とスレッドの数を、機械によらず同じにする。
- 画素を比べるテストは、完全な一致を前提にしない。符号値の差の上限で判定する。

## CI

workflow は、3 つある。

| workflow | 契機 | 内容 | runner |
| --- | --- | --- | --- |
| `.github/workflows/ci.yml` | PR、`develop`・`release`・`main` への push | `tools/ci.sh`（書式、lint、テスト） | GitHub の標準の runner |
| `.github/workflows/vm.yml` | `develop`・`release` への push（文書だけの変更を除く）、毎晩、手動 | VM の中の試験（`tools/vm-check.py`） | GitHub の標準の runner |
| `.github/workflows/gpu-probe.yml` | 手動 | GPU の付いたインスタンスの上で、ドライバー、Vulkan、KMS の状態を調べる。合否は、判定しない | GPU の付いた、クラウドのインスタンス |

- `ci.yml` と `vm.yml` は、リポジトリの中のものと、公開されている依存だけで動く。GPU も、秘密の値も、外部のサービスの登録も、要らない。
  fork でも、そのまま動く。**必須の検査は、この 2 つの範囲に置く。**
- `gpu-probe.yml` は、リポジトリの持ち主が用意した runner が要る。fork には、その runner が無いので、ジョブは、待ったままになる。

### `ci.yml`

検査の本体は、`ci` の devShell の中で走らせる `tools/ci.sh` である。devShell の中で走らせる検査と、CI の検査は、同じものである。

| ジョブ | 内容 |
| --- | --- |
| `changes` | 変わったファイルを調べる。文書（`docs/**`、`*.md`）だけの変更なら、`check` を飛ばす。差分が取れないときは、すべてを走らせる |
| `check (lint)` | Nix を入れ、`nix flake check`（devShell と、VM のゲストの定義の評価）と、`nix develop .#ci --command tools/ci.sh lint` を走らせる |
| `check (test)` | Nix を入れ、`nix develop .#ci --command tools/ci.sh test` を走らせる。デバイスは、lavapipe である |
| `result` | 全体の合否をまとめる。`check` の 2 つが成功か、飛ばされていれば、通る。**必須の検査にするのは、これ 1 つである** |

- `check` の 2 つは、並んで走る。かかる時間は、長いほうの 1 つ分である。
- runner は、`ubuntu-24.04` である。GPU は無い。Mesa も、テストが起動するアプリも、runner のものではなく、devShell のものを使う。
- `check (test)` は、検査の前に、runner の `/dev/udmabuf` を、だれでも開けるようにする。
  **開けるようにできなかったときは、`HIZUKURI_TEST_SKIP_UDMABUF=1` を設定して、`/dev/udmabuf` が要るテストを外す。**
  外したことは、ジョブの要約と、警告に出る。
- 同じ PR の古い実行は、新しい push で取り消す。
- 失敗したときは、`target/*/test-output/`（取り込んだ画像と、hizukuri のログ）を、artifact として 14 日残す。

### `vm.yml`

ジョブは、`session` の 1 つである。PR では、走らない（かかる時間が長いためである）。

1. runner の `/dev/kvm` を、使えるようにする。
2. Nix を入れ、`ci` の devShell と、ゲストと QEMU を用意する（`tools/vm/vm build`）。
3. devShell の中で、`hizukuri` と `hizukuri-test-client` をビルドする。
4. `tools/vm/vm test tools/vm-check.py` で、既定の部を回す。
5. VM を、仮想化されていない機械に見える構成で起動し直して、`real-hardware` の部を回す。
6. 結果を、ジョブの要約に書く。項目ごとの結果、ログ、取り込んだ画面、ゲストのシリアルコンソールのログを、artifact として 14 日残す。

- runner は、`ubuntu-24.04` である。KVM を使う。GPU は、使わない。
- `venus` の部は、GPU が要るので、回さない。

### `gpu-probe.yml`

実際の GPU のドライバーの上で、hizukuri に要るものが、どこまで動くかを調べる。**何も判定しない。**
調べた結果を、ジョブの要約と、artifact（14 日）に残す。調べる step が失敗しても、ジョブは、先へ進む。

- 契機は、手動（`workflow_dispatch`）だけである。入力の `gpu` で、`amd`、`nvidia`、`both` を選ぶ。
- runner は、[RunsOn](https://runs-on.com) が、ジョブごとに起動して、終わると消す、AWS のインスタンスである。
  ジョブの `runs-on` のラベルで、インスタンスの種類と image を指定している。秘密の値は、使わない。
- キャッシュは、使わない。step ごとの時間が、何も無い状態から始めた機械での時間になる。
- 準備のうち、システムを変えるもの（パッケージの導入、kernel の module の読み込み、`/dev/dri/` のノードの権限）は、workflow の step が行う。
  インスタンスは、そのジョブのためだけにある。`tools/gpu-report.sh` は、システムを変えない。

| ジョブ | インスタンスと image | 調べること |
| --- | --- | --- |
| `amd` | `g4ad.xlarge`（AMD Radeon Pro V520）、`ubuntu24-full-x64` | 下の表 |
| `nvidia` | `g4dn.xlarge`（NVIDIA T4）、`ubuntu24-gpu-x64`（NVIDIA のドライバーが入っている） | ドライバーの版（`nvidia-smi`）、`/dev/dri/` のノード、`drm_info`、`tools/gpu-report.sh`（image の loader で。次に、devShell の中で、image の NVIDIA のライブラリだけを dynamic linker に見せて）。ビルドとテストは、しない |

`amd` のジョブの step:

| step | 内容 |
| --- | --- |
| Load amdgpu | `linux-modules-extra` と `linux-firmware` を入れて、kernel の `amdgpu` を読み込む。kernel のログのうち、GPU に関わる行を残す |
| Load vkms | 仮想の KMS のドライバー（vkms）を読み込む |
| Describe the Vulkan driver | devShell の中で、`VK_DRIVER_FILES` を、devShell の Mesa の RADV の manifest に向けて、`tools/gpu-report.sh` |
| Describe the KMS devices | `hizukuri kms-info --summary` と、`kms-info --anonymize`。別の step で、`drm_info` |
| Self test on the GPU's own display | amdgpu の card で、`hizukuri kms-selftest` |
| Self test on vkms | RADV で描いて、vkms に出す `kms-selftest`。`--scanout cpu` と `--scanout dmabuf` の 2 回。seat は、libseat の `noop` を使う |
| Run the tests on the GPU | `VK_DRIVER_FILES` を RADV に向け、`HIZUKURI_TEST_DEVICES=gpu` で、`tools/ci.sh test` |

### 共通のこと

- action は、どれも、commit の hash で固定している。
- どの workflow も、`CARGO_INCREMENTAL=0` と `CARGO_PROFILE_DEV_DEBUG=line-tables-only` を設定している。
  ビルドとキャッシュを小さくするためで、検査の内容は変わらない。

### キャッシュ

`ci.yml` と `vm.yml` は、GitHub の cache を使う。保存するのは、branch への push のときだけである。PR は、読むだけである。
どのキャッシュの中身も、公開の場所から取り直すか、ビルドし直せる。無くても、時間がかかるだけである。

| キャッシュ | 鍵 | 中身 | 保存する workflow |
| --- | --- | --- | --- |
| `/nix`（devShell） | `nix-`。`flake.nix`、`flake.lock`、`rust-toolchain.toml` の hash | `ci` の devShell | `ci.yml`（`check (test)`）。`check` の 2 つが読む |
| cargo（lint） | `cargo-lint-`。`flake.lock`、`rust-toolchain.toml`、`Cargo.lock` の hash | registry、git の依存、`target/`（clippy の結果） | `ci.yml` |
| cargo（test） | `cargo-test-`。同じ | registry、git の依存、`target/`（テストのビルド） | `ci.yml` |
| cargo（VM） | `cargo-vm-`。同じ | registry、git の依存、`target/`（dev のプロファイルのビルド） | `vm.yml` |

- cargo のキャッシュは、`Cargo.lock` だけが違うときは、同じ種類の、近いものを読む。
- 3 つの cargo のキャッシュは、ビルドするもの（clippy、テストのプロファイル、dev のプロファイル）が違うので、別に持つ。
- **鍵の名前は、`ci.yml` と `vm.yml` で対になっている。** 片方の鍵の作り方を変えるときは、もう片方も見る。

## 未実装のもの

- 基準画像（PNG）との比較。いまのテストは、画素の値を、計算した期待値と比べている。
- 回数の多い proptest などを回す、夜間の検査。
- 実際の GPU のドライバーでのテストを、決まった契機で回して、合否を判定する workflow。いまあるのは、手動で起動して、状態を調べる `gpu-probe.yml` だけである。
- nested のバックエンドと、そのテスト。
