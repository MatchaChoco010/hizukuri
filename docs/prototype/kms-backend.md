# KMS のバックエンド

検証用の compositor が、仮想コンソールの上の DRM/KMS の session として動くときの作りである。
session は libseat、デバイスの検出は udev、入力は libinput、画面への出力は `drm` crate で、atomic commit を自分で組む。

## 構成

```text
prototype/backend/src/kms/
  model.rs       デバイスの記述（connector、CRTC、plane、property、mode）。JSON で読み書きできる
  plan.rs        純粋な関数。connector、mode、CRTC、primary plane の選び方、atomic commit の組み立て、
                 scanout の経路の選び方
  frame.rs       フレームを描く時機（FramePacer）。flip の統計
  input.rs       libinput のイベントから、共通の入力イベントへの変換。終了と VT 切り替えのキー
  blob.rs        IN_FORMATS、EDID の読み取り。EDID から、機器を特定する情報を消す処理
  clock.rs       差し替えられる時計
  device.rs      trait KmsDevice。ハードウェアに触る口は、これだけ
  mock.rs        模擬のデバイス。kernel の検査を再現する
  drm_info.rs    `drm_info -j` の記録を、デバイスの記述に読み替える
  backend.rs     KmsBackend<D: KmsDevice, C: Clock>。バックエンドの本体
  drm_device.rs  KmsDevice の、本物の実装（`drm` crate）
  session.rs     libseat、udev、libinput を calloop につなぐ
prototype/compositor/src/
  kms.rs         `--backend kms` の起動、`kms-info`、`kms-selftest`
  safety.rs      仮想マシンの中かどうかの判定、起動の条件、watchdog
  selftest.rs    自己診断の本体（バックエンドに対して generic）と、テストパターンを描く PatternRenderer
  targets.rs     出力の画像（proto-gfx-core の上）。スクリプトの描画と、PatternRenderer が使う
  timing.rs      1 フレームにかかった時間の記録
```

ハードウェアに触る部分は、`KmsDevice` の向こうにある（`drm_device.rs` と `session.rs`）。
選び方、commit の組み立て、フレームの時機、入力の変換は、純粋な関数か、模擬のデバイスと差し替えられる時計の上で動く。
テストは、模擬のデバイスに、実際のデバイスの記述（`tests/fixtures/`）を読み込ませて、バックエンドの全体を動かす。

## 動作

- つながっている connector を 1 つ選び、CRTC と primary plane を選んで、モードを設定する。出力は 1 つだけ使う。
  ほかの connector には触れない。
  - connector: `--kms-output` の指定。無ければ、つながっている最初のもの。writeback connector と、
    `non-desktop` の connector（VR のヘッドセットなど）は、名前で指定されない限り選ばない。
  - モード: `--kms-mode` の指定。無ければ、画面が preferred とするもの。
  - CRTC: その connector がいま使っているものを優先する。ほかの connector が使っている CRTC は取らない。
- バッファは 2 枚持つ。最初のフレームと、session の再開の後は、blocking の modeset を行う。以後は page flip で、
  完了のイベントを待ってから、次のフレームを描かせる。
- `Presented` を、kernel が page flip のイベントに付けた時刻で出す。
- 前の DRM master が CRTC に残した plane（カーソルなど）を、最初の modeset で外す。
- session の一時停止（VT を離れる）の間は、何も commit しない。離れる瞬間の flip が `EACCES` で拒否されるのは、
  想定している動作である。再開したら、デバイスを読み直して、modeset からやり直す。
- 一時停止のとき、押されているキーとボタンを、離したものとしてクライアントに伝える（VT を切り替えるキーの
  「離す」は届かないため）。押したことを伝えていないキーの「離す」は、捨てる。
- hotplug は、udev の通知でデバイスを読み直す。使っている connector が抜ければ出力を外し、出力が無いときに
  何かがつながれば出力を作る。
- 終了時に、出力を止め、framebuffer とバッファを解放する。
- カーソルは、plane ではなく、フレームに合成して描く。クライアントが `wl_pointer.set_cursor` で渡した表面を描く。
  cursor-shape で名前を指定されたときは、どの名前でも、内蔵の矢印を描く。
- connector は、session のデバイスでは probe ありで読む。`kms-info` は、probe なしで読む
  （動いている compositor の邪魔をしないため）。

### scanout の 2 つの経路

| 経路 | 内容 | 選ばれる条件 |
| --- | --- | --- |
| `dmabuf` | 描くもの（スクリプトの描画、自己診断の `PatternRenderer`）が Vulkan で確保した画像を、dma-buf として取り出し、そのまま framebuffer にする | 描くもののデバイスが、KMS のデバイスと同じ GPU である（`VK_EXT_physical_device_drm` で突き合わせる）。かつ、描くものが書き出せる modifier と、plane が受け取る modifier に、共通のものがある |
| `cpu-copy` | フレームを読み戻して、dumb buffer に書く | 上が成り立たないとき（lavapipe など） |

- `--kms-scanout dmabuf|cpu` で、経路を固定できる。固定した経路が使えなければ、起動に失敗する。
  既定の `auto` では、共通の modifier が無いときも、dma-buf の画像を作れなかったときも、`cpu-copy` になる。
- 出せる modifier は、ドライバーごとに違う。LINEAR を出さず、自分の tiled の modifier だけを出すドライバーでは、
  plane が LINEAR しか受け取らないとき（vkms など）、`auto` は `cpu-copy` になる。
- 取り出せるかどうかだけでは、経路を決めない。lavapipe も dma-buf の取り出しに対応しているが、
  その画像は、KMS のデバイスのものではない。
- plane に `IN_FORMATS` が無いドライバー（virtio-gpu）では、`DRM_FORMAT_MOD_LINEAR` だけを使い、
  framebuffer を modifier なしで作る。
- modifier が示されていないバッファ（`DRM_FORMAT_MOD_INVALID`）は、modifier を付けずに framebuffer にする。
- scanout の format は `XRGB8888` だけである。
- GBM で確保して取り込む経路は、無い。

## 安全策

| 策 | 内容 |
| --- | --- |
| KMS を、黙って選ばない | `--backend` の既定は `headless`。`--backend kms` を明示したときだけ、KMS を取りに行く。`--backend auto` は、`WAYLAND_DISPLAY` か `DISPLAY` があれば `headless` になる |
| 仮想マシンの外では、黙って動かない | CPU に `hypervisor` フラグが無い機械では、`--allow-real-hardware`、`--kms-device`、`--kms-output` の 3 つがそろわない限り、起動を拒否する（`safety::check_takeover`）。足りないものだけを表示する。拒否の前に、session も画面も取らない。仮想マシンの中では、どれも要らない |
| 自動終了 | `--exit-after 秒`。`kms-selftest` は `--seconds` が必須で、必ず自分で終わる |
| 終了用のキー | Ctrl+Alt+Backspace。libinput のイベントを共通のイベントに変換する最初の段で扱う。クライアントにも、中核にも、スクリプトにも渡る前である |
| VT の切り替え | Ctrl+Alt+F1〜F12。同じ段で扱う |
| watchdog | メインループが 15 秒止まったら、プロセスを abort する（`--watchdog 秒`、0 で無効）。プロセス全体が止められていた場合（SIGSTOP、デバッガー）は、再開しても abort しない |
| シグナル | SIGINT、SIGTERM、SIGHUP で、後始末をして終わる。シグナルは、どのスレッドよりも先に止めて、イベントループで受ける（Vulkan のドライバーのスレッドがシグナルを受けて、プロセスを落とさないように） |

## デバイスの一覧と、自己診断

```sh
proto-compositor kms-info --summary                      # デバイスと、つながっている画面の一覧
proto-compositor kms-info --device /dev/dri/card1 --anonymize --output device.kms.json
```

`kms-info` は、読み取りだけである。DRM master を取らず、何も変えない。ほかの compositor が画面を使っているままで実行できる。
`--device` を付けると、そのデバイスの記述そのものを出す（そのまま、テストの入力になる）。付けなければ、
`{device, snapshot}` の配列を出す。`--summary` は、人が読む一覧を出す（下）。`--anonymize` は、EDID から、画面の機器を特定する情報（製造者、製品コード、製造番号、製造の時期、
名前と製造番号と自由な文字列の descriptor）を消す。モード、色の特性、HDR の metadata は残る。

```text
/dev/dri/card0  driver <ドライバーの名前>  <PCI の番地>  [the seat's primary GPU]
    DP-1         3840x2160 at 60.000 Hz  (in use)
    HDMI-A-1     1920x1080 at 60.000 Hz
/dev/dri/card1  driver <ドライバーの名前>  <PCI の番地>
    no display connected
```

- `[the seat's primary GPU]` は、firmware が画面を出したデバイスである。`--kms-device` を付けないときに使うデバイスでもある。
- `(in use)` は、いま CRTC につながっている画面である。
- `card0`、`card1` の番号は、起動のたびに入れ替わることがある。

```sh
proto-compositor kms-selftest --output Virtual-1 --seconds 10 --report DIR
```

`kms-selftest` は、決めた出力 1 つだけに対して、モード設定、テストパターンの表示、page flip の時機の計測を行い、
指定した秒数で自分で終わる。結果を、`--report` のディレクトリに書く。

- テストパターンは、`PatternRenderer`（`proto_compositor::selftest::PatternRenderer`）が、`proto-gfx-core` の検査つきの GPU の API の上で、
  直接描く。スクリプトは使わない。
- 診断の道具であって、コンポジターの描画ではない。パターンの符号の値を、色の変換をせずに、そのまま出力の画像に書く。
- スクリプトの描画と同じく、Vulkan 1.4 と `VK_EXT_descriptor_heap` が要る。
- デバイスは、`--vulkan-device auto|gpu|cpu|<名前>` で選ぶ。選び方は、compositor のスクリプトの描画と同じである
  （`cpu` と `lavapipe` は llvmpipe。`auto` は、画面を出している GPU を、Vulkan がそれを持っていれば選ぶ）。
- `--scanout` で、scanout の経路を選ぶ（`--kms-scanout` と同じ値）。
- `--no-drawing` では、パターンを描かずに、黒いフレームを出し、時機だけを測る。

| ファイル | 内容 |
| --- | --- |
| `report.json` | 項目ごとの結果、flip の間隔の統計、フレームの時間、パターンを描いた GPU のデバイス（`gpu`: デバイス名、種類、ドライバー、`drm_primary`、`on_kms_device`、`scanout_modifiers`） |
| `summary.txt` | 人が読む要約 |
| `device.json` | デバイスの記述（`--anonymize` と同じ処理をしたもの） |

項目は、`modeset`、`scanout_path`、`frames_presented`、`page_flip_timing`、`frame_contents`、`commits`、`frame_time` である。
`hdr_output`、`variable_refresh`、`tearing` は、未実装として `not_implemented` を返す。

`kms-selftest` にも、上の安全策が同じように効く。仮想マシンの外では、`--allow-real-hardware`、`--device`、`--output` の
3 つが要る。`--seconds` は必須で、上限は 600 秒である。

## 起動と終了

- 仮想コンソールにログインして、起動する。そのコンソールが、いま表示されているコンソールでなければ、
  seat が有効になるのを 10 秒待って、失敗する。SSH の login には seat が無いので、SSH からは session を取れない。
- `WAYLAND_DISPLAY` か `DISPLAY` がある環境で、`--backend kms` を明示して起動すると、警告を出して続ける。
- 終了のしかたは、軽い順に、次のとおりである。どれも、ほかの connector の状態には触れない。

| 方法 | 何が起きるか |
| --- | --- |
| `--exit-after`（自己診断では `--seconds`）の時間が過ぎる | 後始末をして終わる |
| Ctrl+Alt+Backspace | 後始末をして終わる |
| 制御口の `quit` | 後始末をして終わる |
| SIGINT、SIGTERM、SIGHUP | 後始末をして終わる。出力を止め、socket を消す |
| メインループが止まったまま、watchdog の時間が過ぎる | プロセスを abort する。kernel が DRM のデバイスを閉じ、コンソールが文字の表示に戻る |
| SIGKILL | 後始末なしで終わる。kernel が DRM のデバイスを閉じる。socket は残るが、次の起動は、残った socket を消して起動する |

- Ctrl+Alt+Backspace と、Ctrl+Alt+F1〜F12 が効くのは、メインループが動いているときだけである（キーを、自分で読んで処理する）。
- 終了するときに、出力を止める。元のモードには戻さない。戻すのは、次に画面を取るもの（別の compositor、kernel のコンソール）である。
- 止まった session のいるコンソールへ切り替えても、画面は描き直されない（前に表示していたものが残る）。

## VM での検査

DRM/KMS の session としての動作は、検証用 VM の中で、`tools/vm-check.py` が検査する。
回し方と、部（`--only`）の一覧は、[検証用 VM](../dev/vm-environment.md)にある。
スクリプトは、VM の外で実行する。ビルド、ゲストへのバイナリの送付、検査、結果の回収までを行う。

```sh
tools/vm/vm test tools/vm-check.py --only lavapipe --skip-build     # ビルドと送付を省く
tools/vm/vm test tools/vm-check.py --out artifacts/vm-check/run1    # 記録を残す場所を決める
```

`lavapipe` の部は、次の項目を検査する: 仮想コンソールからの起動、描画のデバイス（`state` の `gpu`）と scanout の経路、モード、
背景とウィンドウの画素（VNC から読む）、libinput 経由のポインター・クリック・キー、VT の切り替えと復帰、
実際のアプリ（foot、weston-terminal）、フレームの時間、終了用のキー、自動終了、制御口からの終了、SIGHUP、
dmabuf を送るクライアントと popup と selection、
libinput からの相対的な動きと pointer lock（加速を掛けた後と前の値、lock の間に `wl_pointer.motion` が無いこと）、
wl-clipboard が data control でクリップボードと primary selection を読み書きすること、スクリプトの層、自己診断。

- compositor は、lavapipe の上で、スクリプトで描く。スクリプトのウィンドウ管理を確かめる項目のほかは、
  描くだけのプロジェクト（`prototype/compositor/projects/draw-only/`）で動かし、ウィンドウを、項目が置いた場所に保つ。
- 自己診断の項目は、`kms-selftest` を動かす。パターンは、`proto-gfx-core` で描く。

- FAIL が 1 つも無ければ、終了コードは 0 である。
- 原因を切り分けるための記録（compositor のログ、制御口から取った状態、画面の PNG、クライアントの出力、
  自己診断の報告）は、`--out` のディレクトリ（既定は `artifacts/vm-check/<日時>/`。git では管理しない）に残る。
- ゲストには、`foot`、`weston-terminal`、`vkcube`、`wl-clipboard`、`alacritty` が要る。無いアプリの項目は、SKIP になる。

## 未実装のもの

- 複数の出力を同時に使うこと。
- direct scanout、overlay plane、cursor plane。virtio-gpu の cursor plane は、`DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` を
  宣言しないクライアントには見えない（検証用の compositor は宣言していない）。
- HDR の出力、10 bit 以上の scanout、VRR、ティアリング（async page flip）。
- `IN_FENCE_FD` による、GPU の完了の受け渡し。
- writeback connector を使った、表示した画素の取り出し。
