# テストと CI

検査の構成と、回し方をまとめる。
テストごとの環境変数と、起動時の指定は、[検証用の compositor](../prototype/README.md)にある。

## 考え方

- **テストは、画面なし（headless）と、CPU で描く Vulkan（lavapipe）で動くものを、基本にする。**
  仮想コンソールも、ディスプレイも、GPU も、要らない。必須の CI も、この範囲で回す。
  テストは、Vulkan 1.4 と `VK_EXT_descriptor_heap` を持つドライバーなら、どれでも動く。
- **ハードウェアに触る部分を、薄くする。** KMS のバックエンドは、DRM のデバイスに触る部分を、trait の向こうに置いている。
  判断のロジック（モードと plane の選び方、atomic commit の組み立て、フレームを描く時機）は、模擬のデバイスでテストする。
- **DRM/KMS の session としての動作は、[検証用の VM](vm-environment.md)の中で、自動で確かめる。**
- **色は、見た目ではなく、数値で確かめる。** 読み戻した画素を、CPU の参照計算と比べる。
- **Vulkan の仕様が、ドライバーに任せていることを、テストで決め打ちにしない。**
  - 浮動小数点から UNORM への変換は、`f × (2^b − 1)` に近い 2 つの符号値の、どちらを返してもよい（最も近いほうへの丸めは推奨で、
    積がちょうど整数のときだけ、その値に決まる）。0.5 は、8 bit で 127 にも 128 にもなる。8 bit の値を比べるときは、±1 を許す。
  - scanout の画像の format modifier は、ドライバーごとに違う（LINEAR を出さず、tiled のものだけを出すドライバーがある）。
    テストは、描くもの（スクリプトの描画）が書き出せると答えた modifier を使い、LINEAR を前提にしない。
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
tools/ci.sh test      # cargo test --workspace（失敗しても、すべてのテストを最後まで回す）と、プロセスが残っていないことの確認

nix develop --command tools/ci.sh           # devShell の中で。CI と同じ検査
```

変更を出す前に、`tools/ci.sh` を通す。CI が走らせるのも、同じスクリプトである。

| 環境変数 | 既定 | 内容 |
| --- | --- | --- |
| `HIZUKURI_TEST_DEVICES` | `lavapipe` | テストに使う Vulkan のデバイス（`lavapipe`、`gpu`。コンマで並べる）。並べたデバイスが無ければ、テストは失敗する（飛ばさない）。`gpu` は、loader に見えている、CPU でないデバイスを使う |
| `HIZUKURI_TEST_REAL_CLIENTS` | `weston-terminal,wl-clipboard,foot,script-tiles` | 実際のアプリを使うテストで、使うもの。並べたものに要るプログラムが `PATH` に無ければ、失敗する。並べていないもののテストは、飛ばす |
| `HIZUKURI_TEST_SKIP_UDMABUF` | なし | `1` にすると、`/dev/udmabuf` が要るテストを外す。設定しなければ、`/dev/udmabuf` を開けないときに、それらのテストは失敗する。外したことは、`tools/ci.sh` の出力に出る |
| `HIZUKURI_TEST_VALIDATION` | なし | `1` にすると、validation layer を有効にして走らせ、エラーが 0 であることも確かめる |
| `HIZUKURI_ICD_DIRS` | なし | テストが Vulkan のドライバーの manifest を探すディレクトリ（`:` で区切る）。システムの場所より先に探す。探すのは、テストの共通の部品（`prototype/compositor/tests/common/icd.rs` の `find_icd_manifest`）である |
| `CARGO_BUILD_JOBS` | `8` | 並列にビルドする数 |

- Vulkan を使うテストは、`HIZUKURI_TEST_DEVICES` に並べたデバイスのそれぞれで走る。
  lavapipe では、テストが、`VK_DRIVER_FILES` を lavapipe の manifest に向けた子プロセスを起動する。環境の設定は、要らない。
- **GPU なしで dma-buf を扱うテストは、kernel の `/dev/udmabuf` を使う**（lavapipe が、dma-buf の書き出しと取り込みに使う）。
  テストを走らせる利用者が、このデバイスを開けることが要る。開けるようにできない環境では、`HIZUKURI_TEST_SKIP_UDMABUF=1` で外す。
  対象のテストと、`HIZUKURI_TEST_REAL_CLIENTS` に並べられる名前は、[検証用の compositor](../prototype/README.md)にある。
- 実行バイナリを使うテストは、テストごとに、専用の `XDG_RUNTIME_DIR` を作り、終わったら消す。
- 取り込んだ画像と、`proto-compositor` のログは、`target/test-output/<テスト名>-<デバイス>/` に残る
  （devShell の中では、`$CARGO_TARGET_DIR/test-output/`）。
- **`tools/ci.sh` は、最後に、テストが起動したプロセスが残っていないことを確かめる。**
  実行ごとに違う値の環境変数（`HIZUKURI_TEST_RUN`）を付けてテストを走らせ、その値を引き継いだプロセスを探す。
  プロセスを名前では探さず、何も止めない。残っていれば、PID を表示して失敗する。

## テストの構成

| 種類 | 場所 | 内容 | Vulkan |
| --- | --- | --- | --- |
| 書式と lint | `tools/ci.sh lint` | `cargo fmt --check`、`cargo clippy` | 不要 |
| 色の計算 | `proto-color` の単体テスト | 原色の行列と transfer function を、規格の値と比べる。往復で、元の値に戻ること | 不要 |
| fallback | `proto-compositor` の `fallback/` の単体テスト | ずらした配置の位置と、2 つの出力の上での、ウィンドウと出力の出来事の列に対する、ウィンドウの表（`fallback::Windows`）の振る舞いを、具体的な期待値で確かめる。当たり判定 | 不要 |
| プロトコル | `prototype/compositor/tests/protocol.rs`、`tests/input_protocols.rs`、`tests/color_management.rs`、`proto-wl` の `tests/server.rs`、`tests/ime.rs` | compositor と検証用のクライアントを、同じスレッドで動かす。何も描かない `NullScene` と、fallback のウィンドウ管理（`Compositor::new`）。configure、focus、キー、ポインター、バッファの解放、subsurface。相対的な動き、ポインターの制約、primary selection、data control。入力メソッド（検証用のクライアントが、入力欄のあるアプリと IME の両方を演じる）: activate と deactivate、grab、preedit と commit、仮想キーボード、IME が落ちたとき、候補ウィンドウの位置、grab の間のメニュー。`wp_color_management_v1`: bind した版ごとの公開の内容、image description の作成と、commit で有効になること、出力の記述と、その変化の通知、望まれる記述、仕様が定めるエラー | 不要 |
| xwayland-satellite の要求 | `proto-wl` の `tests/xwayland_satellite.rs` | xwayland-satellite が送るのと同じ順の要求（ダイアログ、大きさの制限、X11 のメニューの popup、xdg-activation）を、検証用のクライアントが送る | 不要 |
| X11 の display | `prototype/compositor/src/x11.rs` の単体テスト、`prototype/compositor/tests/xwayland_satellite.rs` | display の番号と lock、socket。最初の接続で、xwayland-satellite の代わり（`proto-test-client --as-xwayland-satellite`）が起動すること、渡される引数と socket と signal mask、終わった後の起動し直し、起動できないとき。socket はテストのディレクトリに作る | 要る（compositor を起動するため） |
| バックエンドの取り決め | `prototype/compositor/tests/backend_contract.rs` | 中核を、台本どおりにイベントを出すバックエンドで動かす。`NullScene` と、fallback のウィンドウ管理。出力の追加と削除、フレームの要求、session の一時停止と再開 | 不要 |
| KMS のロジック | `proto-backend` の単体テスト、`tests/kms_mock.rs`、`tests/kms_fixtures.rs` | 模擬の DRM のデバイスで動かす（下の節） | 不要 |
| KMS の scanout | `prototype/compositor/tests/kms_scanout.rs` | 中核と KMS のバックエンドを、模擬のデバイスの上で、スクリプトの描画とつなぎ、模擬の「画面」の画素を確かめる。模擬の plane が受け取る modifier を、スクリプトの描画が書き出せるものから作り、選ばれる scanout の経路を確かめる。共通の modifier が無ければ、`auto` は CPU コピーになり、`dmabuf` の指定は失敗する（スクリプトの描画が tiled の modifier だけを出す場合は、`TiledOnly` で作る）。模擬のデバイスの上の自己診断（`PatternRenderer`）。テストが Vulkan のドライバーの manifest を探す順 | 要る |
| headless の全体 | `prototype/compositor/tests/headless.rs` | 実行バイナリを headless で起動し、検証用のクライアントを出し、画面を取り込んで、画素を数値で確かめる。入力も確かめる。どの場面も、Vulkan のデバイス（`HIZUKURI_TEST_DEVICES`）ごとに 1 回走る。スクリプトが描く（描くだけのプロジェクトで、配置は fallback）。色は、`proto-color` の CPU の参照計算と比べる | 要る |
| スクリプトの層 | `prototype/compositor/tests/script.rs`、`tests/script_window_requests.rs` | 混ざった場面を 4 つの出力に描き、CPU の参照計算と比べる（下の「色の検証」）、作業用の色空間、dmabuf の取り込み、変形したウィンドウへの入力、タイル、スクリプトの故障と読み直し、入力メソッドの候補ウィンドウ、`wp_color_management_v1` で設定した色の画素。ダイアログを浮かせること、fullscreen、activate の要求 | 要る |
| スクリプトに見せる GPU の API | `prototype/gfx-core/tests/`、`prototype/gfx-lua/tests/` | 誤った呼び出しが、エラーの値になること。dmabuf の書き出しと取り込み。`gfx-core` のテストの多くは、shader を、源の入口と、SPIR-V の入口（naga が書いた SPIR-V）の両方で作って、同じ結果になることを確かめる。SPIR-V の入口の、添字の切り詰めと、拒むべき module と、1 語ずつ書き換えた module（`spirv_input.rs`。全 bit を試す版は `--ignored`） | 要る |
| Slang の shader | `prototype/slangc/tests/` | `slangc` が書いた SPIR-V を、GPU の API の SPIR-V の入口に通す。reflection、描画、texture の配列、`discard`、module。`slangc` のエラーの行と列、時間の上限 | 要る |
| 実際のアプリ | `prototype/compositor/tests/real_clients.rs`、`tests/script_real_clients.rs` | 実際のアプリを、headless の中で動かす。スクリプトの描画（`real_clients.rs` は、描くだけのプロジェクト）。popup と grab、selection、fractional-scale、dmabuf | 要る |

実行バイナリを起動するテスト（`prototype/compositor/tests/common/mod.rs` の `Session`）は、`Session::start` では、テストが `--script` を指定しない限り、
描くだけのプロジェクト（`prototype/compositor/projects/draw-only/`）を `--script` に渡して起動する。ウィンドウは、テストが置いた場所に残る。
`Session::start_builtin` は、組み込みのプロジェクト（ウィンドウをタイルに並べる）で起動する。
`session` を読んで、タイルに並べないはずのテスト用のプロジェクトは、`session` のモジュールとして、描くだけのプロジェクトの `session.luau` を持つ。

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

- `proto-color` は、純粋な計算である。原色から求めた行列と、transfer function を、規格の値と比べる。
  CPU の参照計算（デコード、合成、エンコード、量子化）も、ここにある。
- スクリプトの描画のテスト（`script.rs` の `a_mixed_scene_matches_the_cpu_reference`）は、1 つの場面を、4 つの出力に描く。
  場面は、subsurface を持つ不透明なウィンドウ、その上に重なる、不透明度を付けた半透明の Display P3 のウィンドウ、カーソルである。
  出力は、sRGB の gamma 2.2、Display P3 の 16 bit、BT.2100 PQ の 16 bit、scale 1.5 の 4 つである。
  読み戻すのは、合成用の浮動小数点のバッファ（linear light）と、出力の符号化後の画像の 2 つである。
  - 浮動小数点のバッファは、参照計算との差（絶対誤差と相対誤差）で判定する。
  - 符号化後の画像は、参照計算を量子化した値との差が、符号値で決まった範囲に入ることで判定する。
- headless の全体のテストは、検証用のクライアントが単色のバッファを出し、表面の image description を、制御口の `set_surface_color` で指定する。
  指定した情報が、スクリプトの描画まで届いていることを、出力の画素で確かめる。
- `script.rs` は、検証用のクライアントが `wp_color_management_v1` で設定した表面を、制御口で同じ記述を指定した表面と並べて描き、
  合成用のバッファの値を、`proto-color` の参照計算と比べる（Display P3、BT.2100 PQ、BT.709 の linear、Windows の scRGB、HLG、BT.1886、
  数値で与えた原色と指数と輝度）。`wl_shm` は 8 bit なので、範囲の外の値（scRGB の負の値や 1.0 を超える値）は、画素では確かめない。

### KMS のロジックのテスト

KMS のバックエンドは、DRM のデバイスに触る部分を、trait の向こうに置いている。
テストでは、その向こうを、kernel の決まり（atomic commit の検査など）を再現する、模擬のデバイスに差し替える。

- 模擬のデバイスは、実際のデバイスの記録から作る。記録は、`prototype/backend/tests/fixtures/` にある
  （amdgpu、virtio-gpu、vkms のドライバーのもの）。`drm_info -j` の記録（`tools/vm/reference/`）も、読める。
- 記録は、次のコマンドで取る。デバイスを読み取り専用で開き、DRM master を取らず、何も変えない。
  `--anonymize` は、EDID から、モニターを特定できる情報（製造者、製品コード、製造番号、製造の時期、名前）を消す。
  リポジトリに入れる記録は、必ず `--anonymize` を付けて取る。

```sh
proto-compositor kms-info --device /dev/dri/cardN --anonymize
```

- 模擬のデバイスは、framebuffer のメモリを、画素として見せる（dumb buffer と、map できる LINEAR の dma-buf）。
  tiled の modifier の dma-buf は、並びがドライバーのものなので、画素として見せない。
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
- 既定で回るのは、GPU の要らない部である（`lavapipe`、`script`、`vkms`、`safety`、`outputs`、`ime`、`x11`）。
  VM の起動のしかたが違う部（`real-hardware`）は、`--only` で、別に回す。構成が合わない VM に頼むと、失敗になる。

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
| GPU のドライバーでの、Vulkan から KMS への経路 | VM の検査は、GPU を使わない。ゲストは、いつも CPU（lavapipe）で描く |
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
| `.github/workflows/ci.yml` | PR、`develop`・`release`・`main`・`release-candidate/**` への push | `tools/ci.sh`（書式、lint、テスト） | GitHub の標準の runner |
| `.github/workflows/vm.yml` | `develop`・`release`・`release-candidate/**` への push、毎晩、手動 | VM の中の試験（`tools/vm-check.py`） | GitHub の標準の runner |
| `.github/workflows/gpu.yml` | 毎晩、`release-candidate/**` への push、手動 | GPU の付いたインスタンスの上で、ハードウェアのドライバーを使って、`tools/ci.sh test` と、vkms での自己診断を走らせる。合否を判定する | GPU の付いた、クラウドのインスタンス |

- `ci.yml` と `vm.yml` は、リポジトリの中のものと、公開されている依存だけで動く。GPU も、秘密の値も、外部のサービスの登録も、要らない。
  fork でも、そのまま動く。**必須の検査は、この 2 つの範囲に置く。**
- `gpu.yml` は、リポジトリの持ち主が用意した runner が要る。必須の検査ではない。
  PR では、起動しない。fork からの PR が、GPU のインスタンスを起動できないようにするためである。
  fork には、その runner が無い。`gpu.yml` のジョブは、このリポジトリでだけ走り、fork では飛ばされる。

### 文書だけの変更と、候補のブランチ

`ci.yml` と `vm.yml` は、文書（`docs/**`、`*.md`）だけの変更では、重い検査（`ci.yml` の `check`、`vm.yml` の `session`）を飛ばす。
飛ばすかどうかは、workflow の契機の `paths` の条件ではなく、各 workflow の最初のジョブ `changes` が、`tools/ci-changes.sh` で決める。
飛ばしたときは、そのことと、変わったファイルの一覧が、`changes` のジョブの要約に出る。

**候補のブランチ（`release-candidate/**`）への push では、変わったファイルによらず、すべての検査を回す。**
候補のブランチは、`develop` の先頭から切るだけで、新しい commit を持たない。`release` に取り込む前に、すべての検査が通ることを確かめる場所である。
契機の `paths-ignore` は、ブランチごとに変えられない。また、新しい commit を持たないブランチの push を、GitHub が、どう判定するかは、
[文書](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#git-diff-comparisons)に定められていない。
そのため、契機では絞らず、ジョブの側で判定する。

| event と ref | `ci.yml` | `vm.yml` |
| --- | --- | --- |
| `pull_request` | PR の 3 点の差分（`base...HEAD`）が文書だけなら、`check` を飛ばす | 起動しない |
| `develop`・`release` への push | push の 2 点の差分（`before..HEAD`）が文書だけなら、`check` を飛ばす | 同じ判定で、`session` を飛ばす |
| `main` への push | `develop` と同じ | 起動しない |
| `release-candidate/**` への push | 必ず、すべてを回す | 必ず、すべてを回す |
| ブランチを作った push、差分が取れない push（force push で、前の commit が無い） | すべてを回す | すべてを回す |
| `schedule`、`workflow_dispatch` | 契機に無い | すべてを回す |

- `release` への push は、候補を取り込んだ merge commit である。差分は、前回のリリースから今回までの全体になる。
  そこが文書だけのときに飛ばしても、同じ内容の tree を、候補のブランチで、すべての検査が通している。
- `ci.yml` の `result` は、`release-candidate/**` では、`check` が飛ばされていれば、失敗する。

### 候補のブランチの検査の結果を確かめる

`tools/ci-status.sh` は、ある branch のある commit について、検査の workflow の結果を一覧にする。
`gh`（ログインしたもの）で、GitHub の API を読むだけで、workflow を起動しない。

```sh
tools/ci-status.sh release-candidate/<名前>          # いまの結果
tools/ci-status.sh --wait release-candidate/<名前>   # すべての run が終わるまで待ってから
```

- 見るのは、その branch への push で、その commit について起動した run のうち、いちばん新しいものである。
  workflow は、表示の名前ではなく、ファイルの名前で引く。
- **すべての workflow が `passed` のときだけ、0 で終わる。** そうでなければ 1、引数の誤りや API の失敗では 2 で終わる。

| 状態 | 意味 |
| --- | --- |
| `passed` | run が成功し、その run（いちばん新しい試行）のジョブが、すべて成功した |
| `failed` | run が、成功以外で終わった（失敗、取り消し、時間切れなど） |
| `incomplete` | run は成功したが、飛ばされたか、成功しなかったジョブがある（fork で飛ばされた `gpu.yml` のジョブ、文書だけの変更で飛ばした `check` など） |
| `running` | run が、終わっていない |
| `not-started` | その branch への push で、その commit について起動した run が無い |
| `missing` | リポジトリに、その workflow が無い |

| 指定 | 既定 | 内容 |
| --- | --- | --- |
| `--commit SHA` | GitHub の上の、branch の先頭 | 見る commit |
| `--workflows "A B"` | `ci.yml vm.yml gpu.yml` | 見る workflow のファイルの名前 |
| `--wait` | なし | 終わっていない run があれば、終わるまで待つ |
| `--interval 秒` | `30` | 待つあいだに、見直す間隔 |
| `--start-timeout 秒` | `300` | 待つあいだに、run の無い workflow（`not-started`、`missing`）を、起動するまで待つ時間 |
| `--timeout 秒` | `14400` | 待つ時間の上限。超えたら、その時点の結果を出す |

### `ci.yml`

検査の本体は、`ci` の devShell の中で走らせる `tools/ci.sh` である。devShell の中で走らせる検査と、CI の検査は、同じものである。

| ジョブ | 内容 |
| --- | --- |
| `changes` | `tools/ci-changes.sh` で、`check` を飛ばすかどうかを決める（上の節） |
| `check (lint)` | Nix を入れ、`nix flake check`（devShell と、VM のゲストの定義の評価）と、`nix develop .#ci --command tools/ci.sh lint` を走らせる |
| `check (test)` | Nix を入れ、`nix develop .#ci --command tools/ci.sh test` を走らせる。デバイスは、lavapipe である |
| `result` | 全体の合否をまとめる。`check` の 2 つが成功か、飛ばされていれば、通る。`release-candidate/**` では、成功だけが通る。**必須の検査にするのは、これ 1 つである** |

- `check` の 2 つは、並んで走る。かかる時間は、長いほうの 1 つ分である。
- runner は、`ubuntu-24.04` である。GPU は無い。Mesa も、テストが起動するアプリも、runner のものではなく、devShell のものを使う。
- `check (test)` は、検査の前に、runner の `/dev/udmabuf` を、だれでも開けるようにする。
  **開けるようにできなかったときは、`HIZUKURI_TEST_SKIP_UDMABUF=1` を設定して、`/dev/udmabuf` が要るテストを外す。**
  外したことは、ジョブの要約と、警告に出る。
- 同じ PR の古い実行は、新しい push で取り消す。ブランチへの push では、push した commit ごとに実行を持ち、後の push が、前の実行を取り消すことは無い。
- 失敗したときは、`target/*/test-output/`（取り込んだ画像と、`proto-compositor` のログ）を、artifact として 14 日残す。

### `vm.yml`

ジョブは、`changes` と `session` の 2 つである。PR では、走らない（かかる時間が長いためである）。
`changes` は、`ci.yml` と同じく、`tools/ci-changes.sh` で、`session` を飛ばすかどうかを決める（上の節）。
`session` は、次のことを行う。

1. runner の `/dev/kvm` を、使えるようにする。
2. Nix を入れ、`ci` の devShell と、ゲストと QEMU を用意する（`tools/vm/vm build`）。
3. devShell の中で、`proto-compositor` と `proto-test-client` をビルドする。
4. `tools/vm/vm test tools/vm-check.py` で、既定の部を回す。
5. VM を、仮想化されていない機械に見える構成で起動し直して、`real-hardware` の部を回す。
6. 結果を、ジョブの要約に書く。項目ごとの結果、ログ、取り込んだ画面、ゲストのシリアルコンソールのログを、artifact として 14 日残す。

- runner は、`ubuntu-24.04` である。KVM を使う。GPU は、使わない。
- push した commit ごとに実行を持つ。後の push が、前の実行を取り消すことは無い。

### `gpu.yml`

実際の GPU のドライバーの上で、テストが通ることを確かめる。**合否を判定する。** step が 1 つでも失敗すれば、ジョブは失敗する。
必須の検査ではない。

- 契機は、3 つある。
  - 毎晩（UTC の 18:47）。既定の branch で走る。
  - `release-candidate/**` への push。`release-candidate/**` は、`release` に取り込む commit で、`develop` から切る branch である。
    `release` に merge する前に、そこで、この検査を回す。
  - 手動（`workflow_dispatch`）。どの branch でも選べる。
- runner は、[RunsOn](https://runs-on.com) が、ジョブごとに起動して、終わると消す、AWS のインスタンスである。
  ジョブの `runs-on` のラベルで、インスタンスの種類、image、ディスクの大きさを指定している。秘密の値は、使わない。
- ジョブは、`nvidia` の 1 つである。インスタンスは `g4dn.xlarge`（NVIDIA T4）、image は `ubuntu24-gpu-x64`（NVIDIA のドライバーが入っている）。
  ジョブは、このリポジトリ（`MatchaChoco010/hizukuri`）でだけ走る。
- キャッシュは、使わない。何も無い状態から始めても、実行は短く、起動するのは一晩に 1 回だからである。
- 同じ ref の実行は、重ねない。走っている実行は、取り消さない。

**ドライバー。** image に入っている NVIDIA のドライバーは、`VK_EXT_descriptor_heap` を持たない。
そこで、workflow の環境変数 `NVIDIA_DRIVER` に書いた版（`595.104.02`）を、`download.nvidia.com` の installer で入れ直す。
595 の系列は、`VK_EXT_descriptor_heap` を持つ最初の系列である。検査に使う版は、この値を書き換えて、意図して変える。
手動で起動するときは、入力の `nvidia_driver` で、別の版を指定できる。空なら、`NVIDIA_DRIVER` の版を入れる。

**devShell からドライバーへの届き方。** devShell は、NVIDIA のドライバーを持たない。ドライバーは、インスタンスの kernel の module と、版が合っている必要がある。
そこで、インスタンスに入っている NVIDIA のライブラリだけを、1 つのディレクトリに集め、devShell の中で、`LD_LIBRARY_PATH` をそこに向ける。

- ドライバーの manifest は GLX のライブラリ（`libGLX_nvidia.so.0`）を指していて、これは X11 のライブラリを要る。
  `VK_DRIVER_FILES` は、manifest の、ライブラリの名前を `libEGL_nvidia.so.0` に替えた写しに向ける。
- ドライバーは、動いているあいだに、自分のもののほかのライブラリを開く。`LD_LIBRARY_PATH` には、
  `flake.lock` の nixpkgs の `libdrm`、`libgbm`、`libx11`、`libxext`、`wayland` の `lib` も足す。
  `libdrm` と `libgbm` が無いと、ドライバーは、`VK_EXT_external_memory_dma_buf` を公開しない。
  nixpkgs から取るのは、ドライバーのほかに、システムのライブラリを読み込まないためである。
- devShell は、起動するときに、`LD_LIBRARY_PATH` を外し、`VK_DRIVER_FILES` を設定する。
  そこで、この 2 つを設定する行を、ファイル（`$RUNNER_TEMP/nvidia/settings.sh`）に書き、devShell の中の step が、それを読み込む。

| step | 内容 |
| --- | --- |
| Replace the NVIDIA driver | image のドライバーのパッケージと、kernel の module を外し、NVIDIA の installer（`--silent -M=open`）で、上の版を入れる。installer のログを残し、失敗したときは、その終わりを出す。入れた後の `/proc/driver/nvidia/version` と `nvidia-smi` を出す |
| Load vkms | 仮想の KMS のドライバー（vkms）を読み込む |
| Make the DRM nodes usable | `/dev/dri/` のノードを、だれでも開けるようにし、vkms の card を探す。見つからなければ、失敗する |
| Gather the libraries of the NVIDIA driver | ドライバーの manifest を探し、ドライバーのライブラリを 1 つのディレクトリに集め、`libEGL_nvidia.so.0` を指す manifest を書く |
| Fetch the dev shell | `ci` の devShell を用意する |
| Write how the dev shell reaches the driver | nixpkgs のライブラリを取ってきて、`settings.sh` を書く |
| Check what the Vulkan driver offers | devShell の中で、`tools/gpu-report.sh`。デバイスが見えないか、`VK_EXT_descriptor_heap` か `VK_EXT_external_memory_dma_buf` が無ければ、失敗する。ドライバーの準備の不備を、テストの前に、それとわかる形で出す |
| Make /dev/udmabuf usable, or leave its tests out | `ci.yml` と同じ。`/dev/udmabuf` を開けるようにし、できなければ、それが要るテストを外す |
| Build the compositor | テストのプロファイルで、`proto-compositor` をビルドする。後のテストは、これを使い回す |
| Self test on vkms | GPU で描いて、vkms に出す `proto-compositor kms-selftest`（`--vulkan-device gpu --scanout cpu`）。テストパターンは、`proto-gfx-core` で、GPU の上で描く。フレームを読み戻して、dumb buffer に写す。seat は、libseat の `noop` を使う。報告の `summary.txt` の 1 行目が `proto-compositor kms-selftest: PASSED` でなければ、失敗する |
| Run the tests on the GPU | `HIZUKURI_TEST_DEVICES=gpu` で、`tools/ci.sh test`。必須の CI と同じスクリプトを、ハードウェアのデバイスで走らせる。自己診断だけが失敗したときも、走る |
| Summary | ジョブの要約に、ドライバーの版、`tools/gpu-report.sh` の Vulkan の報告（`vulkan-summary.md`）、自己診断の `summary.txt`（無ければ、ログの終わり）を書く |

- `--scanout dmabuf`（GPU の image を、dma-buf のまま vkms に渡す）は、確かめない。描くデバイスと、出すデバイスが違うので、自己診断が、意図して断る。
- `artifacts/gpu/`（installer のログ、ドライバーの版、DRM のノード、Vulkan の報告、自己診断の報告とログ）と、
  `target/*/test-output/`（テストが取り込んだ画像と、`proto-compositor` のログ）を、artifact `gpu` として 14 日残す。成否によらない。

### 共通のこと

- action は、どれも、commit の hash で固定している。
- どの workflow も、`CARGO_INCREMENTAL=0` と `CARGO_PROFILE_DEV_DEBUG=line-tables-only` を設定している。
  ビルドとキャッシュを小さくするためで、検査の内容は変わらない。

### キャッシュ

`ci.yml` と `vm.yml` は、GitHub の cache を使う。保存するのは、branch への push のときだけである。PR は、読むだけである。
`gpu.yml` は、キャッシュを使わない。
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
- nested のバックエンドと、そのテスト。
