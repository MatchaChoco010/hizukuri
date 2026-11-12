# 検証用の VM

検証用の compositor を、DRM/KMS の session として動かして確かめるための VM である。
ビルド、起動、操作、破棄は、`tools/vm/vm` の 1 つのスクリプトで行う。

- ゲストは、`tools/vm/guest.nix` が定義する NixOS である。`flake.lock` が固定する nixpkgs から作る。
  kernel、Mesa、ゲストの中のアプリの版は、`flake.lock` だけで決まる。
- ゲストは、ディスクを持たない。起動するたびに、同じ状態のゲストになる。
- GPU は、要らない。ゲストは、CPU で描く（lavapipe）。
- 画面を出さずに起動し、SSH、QMP、VNC の Unix socket で操作する。ウィンドウに出して、手で触ることもできる。
- 試験は、1 つのコマンドで、VM の起動から停止まで、自動で回せる（`tools/vm/vm test`）。
  CI も、同じコマンドを使う（[CI](#ci)）。
- QEMU を、一般ユーザーの権限で、直接起動する。`sudo` も、libvirt も使わない。

## 要るもの

| もの | 用途 |
| --- | --- |
| Linux（`x86_64`）と、KVM（`/dev/kvm` を、読み書きできること） | 仮想化 |
| Nix（`nix-build`）。store は、`/nix/store` にあること | ゲストと、道具のビルド |

QEMU、`ssh`、`rsync`、`python3` は、`tools/vm/guest.nix` が、同じ nixpkgs から用意する。機械に入っているものは、使わない。

VM に送るバイナリは、[Nix の devShell](nix-environment.md) の中でビルドする。
ゲストのライブラリは、devShell のものと同じ store のパスにある。devShell の外でビルドしたバイナリは、ゲストで起動しない。

## 作り

| 項目 | 内容 |
| --- | --- |
| 起動 | `qemu-system-x86_64` を、一般ユーザーで直接起動する（`-daemonize`） |
| ゲスト OS | NixOS（`tools/vm/guest.nix`）。kernel は、nixpkgs の `linuxPackages_latest` |
| 起動の方法 | boot loader を使わない。QEMU が、ゲストの kernel と initrd を、直接読み込む（`-kernel`、`-initrd`） |
| ファイルシステム | root は、tmpfs（メモリの上）。`/nix/store` は、VM を動かす機械の store を、9p で、読み取り専用に共有する |
| CPU、メモリ | 4 vCPU（`-cpu host`、KVM）、6 GiB |
| 表示のデバイス | `virtio-vga`。ゲストには、virtio-gpu の KMS のデバイスが見える。出力は 2 つで、2 つ目は、未接続で始まる |
| ゲストの描画 | lavapipe（CPU）。`HIZUKURI_VM_GPU=venus` で起動すると、Venus のデバイスも見える（[Venus](#venus)） |
| 表示 | 画面なし: `-display none` と、Unix socket の VNC。`start --gtk` で、GTK のウィンドウ |
| 入力 | `virtio-keyboard-pci`、`virtio-tablet-pci`（絶対座標）、`virtio-mouse-pci`（相対移動） |
| ネットワーク | user-mode。`127.0.0.1` の空いているポート（22044 から探す）を、ゲストの SSH へ転送する |
| 利用者 | `hizukuri`（uid 1000。`sudo` は、パスワードなし。コンソールのログインのパスワードは `hizukuri`）。SSH は、VM 専用の鍵だけを受け付ける |
| 制御 | QMP の Unix socket、VNC の Unix socket。TCP のポートは、SSH の転送のほかに開けない |

- ゲストに書いたものは、VM を止めると消える。home も、tmpfs の上にある。
- VM 専用の SSH の鍵は、`build` が作る。公開鍵は、起動のときに、QEMU の `fw_cfg` でゲストへ渡す。
  ゲストは、起動のたびに、SSH の host key を作り直す。`start` は、`known_hosts` を消してから起動する。
- ゲストの時刻帯は、UTC である。

### ゲストに入っているもの

| もの | 内容 |
| --- | --- |
| Mesa | lavapipe と、virtio-gpu のドライバー（Venus、virgl）。Vulkan の loader は、lavapipe と Venus の 2 つだけを見る（`VK_DRIVER_FILES`） |
| アプリ | `foot`、weston（`weston-terminal`）、`alacritty`、`vulkan-tools`（`vkcube`、`vulkaninfo`）、`wl-clipboard` |
| 機器と入力を調べる道具 | `drm_info`、`libinput`、`evtest`、`wayland-utils`、`wev` |
| そのほか | `gdb`、`strace`、`rsync`、`vim`、validation layer、Python（libevdev つき）、等幅のフォント、カーソルのテーマ |
| session | logind と polkit。seatd も入っている（自動では起動しない。`tty-run --seatd` が起動する） |
| 補助のスクリプト | `tools/vm/guest/` のもの（`tty-run`、`hzk`、`vkms-load`、`freeze-main-loop`、`uinput-touchpad.py`）。ゲストの PATH にある |

ゲストに何かを足すには、`tools/vm/guest.nix` を書き換える。次の `start` が、ゲストを作り直す。

### 環境変数

`start` が読む。

| 環境変数 | 既定 | 内容 |
| --- | --- | --- |
| `HIZUKURI_VM_DIR` | `$XDG_STATE_HOME/hizukuri-vm`（`XDG_STATE_HOME` が無ければ、`~/.local/state/hizukuri-vm`） | 鍵、ログ、socket を置く場所。ほかのコマンドも、これを読む |
| `HIZUKURI_VM_GPU` | `none` | `none` は、ゲストが CPU で描く。`venus` は、ゲストに Venus のデバイスを見せる |
| `HIZUKURI_VM_OUTPUTS` | `2` | virtio-gpu の出力の数 |
| `HIZUKURI_VM_CPU` | `host` | CPU の model。`host,-hypervisor` で、ゲストを、仮想化されていない機械に見せる |
| `HIZUKURI_VM_CPUS`、`HIZUKURI_VM_MEMORY` | `4`、`6G` | CPU の数、メモリ |
| `HIZUKURI_VM_XRES`、`HIZUKURI_VM_YRES` | `1280`、`800` | 出力の最大の大きさ |
| `HIZUKURI_VM_HOSTMEM` | `4G` | `venus` のときの、virtio-gpu の `hostmem` |
| `HIZUKURI_VM_RENDERNODE` | QEMU が選ぶ | `venus` のときの、表示に使う render node（`/dev/dri/renderD*`） |
| `HIZUKURI_VM_QEMU` | `guest.nix` の QEMU | 別の `qemu-system-x86_64` を使う |

### 作られるもの

| 場所（`HIZUKURI_VM_DIR` の下） | 内容 |
| --- | --- |
| `system`、`host-tools` | Nix の store へのリンク。ゲストの構成と、QEMU などの道具。store の掃除で消えないようにする |
| `id_ed25519`、`id_ed25519.pub`、`known_hosts` | VM 専用の SSH の鍵と、known hosts。`~/.ssh/` は、読みも書きもしない |
| `run/` | QEMU の pid、QMP と VNC の socket、SSH のポート番号、表示と GPU の構成 |
| `log/` | シリアルコンソールのログ（`serial.log`）、QEMU のログ |
| `shots/` | 取り込んだ画面 |

ほかに作られるのは、Nix の store の中のものと、QEMU のプロセス 1 つである。プロセスには、`-name hizukuri-vm` が付く。

## 手順

### ビルド

```sh
tools/vm/vm build
```

ゲストの構成と、道具（QEMU など）を、`nix-build` で用意し、VM 専用の SSH の鍵を作る。

- 必要なものは、Nix の binary cache から取得する（最初は、数 GB）。ここでビルドするのは、このゲストに固有の部分だけである。
- 何度実行しても、同じ結果になる。変更が無ければ、数秒で終わる。
- `start` も、同じことを行う。`build` を、先に実行しなくてもよい。

### 起動と停止

```sh
tools/vm/vm start           # ウィンドウを出さない
tools/vm/vm start --gtk     # GTK のウィンドウに出す
tools/vm/vm wait-ssh        # SSH が通るまで待つ
tools/vm/vm status          # 動いているか、SSH のポート、表示と GPU の構成
tools/vm/vm stop            # ゲストを poweroff する。だめなら QMP の quit、最後に kill
tools/vm/vm stop --force    # すぐ止める
```

- `start` は、動いていれば、何もしない。構成（表示、GPU、出力の数、CPU の model）を変えるには、止めてから、起動し直す。
- ゲストを最初の状態に戻すには、止めて、起動し直す。

### ウィンドウに出して触る

```sh
tools/vm/vm start --gtk
```

QEMU の GTK の表示で起動する。ウィンドウの中に、ゲストのコンソールが出る。

1. 利用者 `hizukuri`、パスワード `hizukuri` でログインする。
2. compositor を起動する。たとえば `hzk`（[検証用の compositor を VM で動かす](#検証用の-compositor-を-vm-で動かす)）、`weston`。
3. ウィンドウをクリックすると、キーボードとマウスが、ゲストに渡る。Ctrl+Alt+G で戻る（QEMU の既定）。
4. 終わったら、ウィンドウを閉じるか、`tools/vm/vm stop`。

- 別の端末から、`tools/vm/vm ssh`、`tools/vm/vm tty-run`、`tools/vm/vm key` などが使える。
- この表示では、`screendump`、`pixels`、`screen-size`、`output`、`move` が使えない（VNC の socket が無い）。
- キーの組み合わせが、外側のデスクトップに取られて、ゲストに届かないときは、
  別の端末から、`tools/vm/vm key ctrl-alt-f2` のように、QMP でキーを送る。

### 破棄

```sh
tools/vm/vm destroy         # 消す対象を表示するだけ。何も消さない
tools/vm/vm destroy --yes   # VM を止めて、HIZUKURI_VM_DIR を消す
```

目印のファイル（`.hizukuri-vm`）が無いディレクトリは、消さない。
Nix の store に取得したものは、store の掃除（`nix-collect-garbage`）で消える。

## 操作のコマンド

| コマンド | 内容 |
| --- | --- |
| `vm build` | ゲストと道具を用意する |
| `vm ssh [CMD...]` | ゲストでコマンドを実行する。引数が無ければ、shell に入る |
| `vm push SRC... DEST`、`vm pull SRC DEST` | `rsync` で、ファイルを送る、取る。`DEST` は、ゲストの home からの相対でよい |
| `vm tty-run [-t VT] [-e NAME=VALUE]... [--seatd] [--wait] CMD...` | ゲストの VT（既定は tty7）で、logind の session として起動する |
| `vm tty-log`、`vm tty-status`、`vm tty-stop` | 出力を読む、状態を見る、止めて tty1 に戻す |
| `vm screendump [--head N] [FILE.png]` | 画面を、PNG に取り込む |
| `vm pixels [--head N] X,Y...` | 画面の画素の、RGB の値を表示する |
| `vm screen-size [--head N]` | いまのモードの大きさ |
| `vm key KEYS...` | キーを押す。`a`、`ret`、`esc`、`ctrl-alt-f2`、`logo-h` など（QEMU の qcode） |
| `vm type TEXT` | ASCII の文字列を打つ |
| `vm move X Y` | ポインターを、画素の絶対座標へ動かす（tablet） |
| `vm rel DX DY` | ポインターを、相対移動する（mouse） |
| `vm click [left]`、`vm button NAME down|up`、`vm scroll up|down [N]` | ボタン、スクロール |
| `vm output HEAD on [WxH]`、`vm output HEAD off` | 出力をつなぐ、外す。`HEAD` は、0 から |
| `vm qmp JSON` | QMP の command を、1 つ送る |
| `vm test [--keep] SCRIPT [ARGS...]` | 起動、SCRIPT の実行、停止を、まとめて行う |

### VT で起動する（`tty-run`）

SSH の session には、seat も VT も無い。そこから DRM/KMS の compositor を起動しても、libseat（logind）が、session を取れない。
`tty-run` は、`systemd-run` と PAM（`pam_systemd`）で、`/dev/ttyN` に結びついた logind の session を作り、その中でコマンドを起動する。
display manager や、コンソールのログインと、同じ形である。

- コマンドは、login shell から起動する。コンソールにログインしたときと、同じ環境変数になる。
- 出力は、ゲストの `~/tty-run.log` に入る。`vm tty-log` で読める。
- 前の実行が残っていれば、止めてから起動する。
- `vm tty-status` は、unit の状態と、`MainPID`（起動したプロセスの PID）、いまの VT を出す。
- `--seatd` は、seatd を起動して、`LIBSEAT_BACKEND=seatd` を設定する。logind の代わりに、seatd を使う経路を試せる。
- `--wait` は、コマンドが終わるまで待ち、その終了コードを返す。

### 画面の取り込み

`vm screendump` と `vm pixels` は、VNC の Unix socket から、1 フレームを取る（`tools/vm/vnc.py`）。

- 取れるのは、ゲストが最後に flip したフレームである。8 bit の RGB で、ゲストが出した値と、そのまま一致する。
- cursor plane の内容も、合成された状態で取れる。
- 出力ごとに、socket がある。`--head N` で、読む出力を選ぶ。
- `start --gtk` で起動した VM では、使えない。

### 入力

| 方法 | 作れる入力 | 経路 |
| --- | --- | --- |
| QMP（`vm key`、`vm move` など） | キー、絶対座標のポインター、相対移動、ボタン、ホイール | 仮想のハードウェア → kernel → libinput。実際の機器と、同じ経路 |
| uinput（ゲストの `uinput-touchpad.py`） | タッチパッドの swipe、指でのスクロール、pinch。書き換えれば、任意の機器 | ゲストの中で、仮想の機器を作る |
| `libinput record` と `libinput replay` | 記録しておいた入力の再生 | `replay` が、uinput で同じ機器を作って、イベントを流す |

- 絶対座標は、同じ座標を続けて送っても、イベントが出ない（kernel が捨てる）。一度、別の場所へ動かす。
- 相対移動には、libinput の加速がかかる。
- `libinput record` は、既定で、キーの code を伏せる。本当の code を記録するには、`--show-keycodes` を付ける。
- uinput の機器を作るには、ゲストの中で root が要る（`sudo uinput-touchpad.py`）。
- uinput の機器は、後から増える。compositor の側は、hotplug で受ける。

## 検証用の compositor を VM で動かす

```sh
nix develop --command tools/vm/try-hizukuri [--gtk] [--skip-build]
```

`try-hizukuri` は、次を行う。devShell の中で実行する。

1. 検証用の compositor をビルドする（`hizukuri` と `hizukuri-test-client`）。`--skip-build` で、省く。
2. VM を起動する（`--gtk` なら、ウィンドウに出す。表示が違う VM が動いていれば、止めて起動し直す）。
3. バイナリ（`hizukuri`、`hizukuri-ctl`、`hizukuri-test-client`）を、ゲストの `~/bin/` へ送る。
   送ったバイナリが、ゲストで起動しなければ、その旨を出して止まる。
4. 組み込みのプロジェクト（`crates/hizukuri/scripts/`）を、ゲストの `~/hzk-scripts.orig/` へ送る。

ゲストは、起動のたびに最初の状態に戻る。VM を起動し直したら、`try-hizukuri` で、送り直す。

### `hzk` で起動する

ゲストの `hzk` は、検証用の compositor を、DRM/KMS の session として起動し、端末をいくつか開く。
VT の上の session から実行する。コンソールにログインして打つか、`tools/vm/vm tty-run -- hzk` とする。

1. コンソールに、利用者 `hizukuri`、パスワード `hizukuri` でログインする。
2. `hzk` と打つ。端末が 3 つ並ぶ。
3. マウスを動かすと、カーソルが付いてくる。ウィンドウをクリックすると、focus が移る。キーを打つと、端末に入る。
4. Ctrl+Alt+F2 で、別のコンソールに切り替わる。元のコンソールの番号の Ctrl+Alt+F*n* で戻る（`hzk` が、起動のときに表示する）。
5. Ctrl+Alt+Backspace で、compositor が終わり、コンソールに戻る。何もしなくても、600 秒で、自分で終わる。

`hzk` の指定:

| 指定 | 内容 |
| --- | --- |
| `lavapipe`（既定） | Rust のレンダラーを lavapipe で動かし、dumb buffer へ写して scanout する |
| `script` | スクリプトの描画と、スクリプトのウィンドウ管理（タイル）。lavapipe で描く |
| `venus` | Rust のレンダラーを Venus で動かし、dma-buf で scanout する。`HIZUKURI_VM_GPU=venus` で起動した VM で使う |
| `--reset-scripts` | `script` と一緒に使う。ゲストで書き換えたプロジェクトを、送ったものに戻す（書き換えたものは、`~/hzk-scripts.bak` に残る） |
| `--seconds N` | 自動で終わるまでの秒数（既定は 600） |
| `--mode 1920x1080` | モード（既定は 1280x800） |
| `--no-apps` | 端末を開かない |

- アプリを足すには、`hzk` が開いた端末の中で、`foot &` と打つ。
  外からは、`tools/vm/vm ssh 'WAYLAND_DISPLAY=hizukuri-0 XDG_RUNTIME_DIR=/run/user/1000 foot' &` とする。
- 制御口は、`tools/vm/vm ssh '~/bin/hizukuri-ctl --socket /run/user/1000/hizukuri-0.control state'` のように使う。
- ログは、ゲストの `~/hzk.log` にある。

### スクリプトの層を触る（`hzk script`）

`hzk script` は、検証用の compositor を、`--renderer script --wm script --script ~/hzk-scripts` で起動する。
画面を描くのも、ウィンドウを並べるのも、`~/hzk-scripts/` のプロジェクトのスクリプトである（Luau。検証用の、仮の言語である）。
端末が 3 つ（`foot`、`weston-terminal`、`alacritty`）、タイルに並ぶ。

| キー | 動作 | 処理するもの |
| --- | --- | --- |
| logo+H、logo+L | 左、右のウィンドウへ、focus を移す | スクリプト（`tile.luau`） |
| logo+shift+H、logo+shift+L | focus のあるウィンドウを、隣と入れ替える | スクリプト |
| logo+Q | focus のあるウィンドウを閉じる | スクリプト |
| Ctrl+Alt+Shift+R | スクリプトを読み直す | コア |
| Ctrl+Alt+Shift+F | スクリプトを外して、組み込みの fallback にする（上の縁に、帯が出る） | コア |
| Ctrl+Alt+Backspace、Ctrl+Alt+F2 | 終了、コンソールの切り替え | コア |

- スクリプトを書き換えて、Ctrl+Alt+Shift+R を押すと、ウィンドウを開いたまま、読み直される。
  たとえば、`~/hzk-scripts/tile.luau` の `T.gap = 8` を変えると、ウィンドウが、新しいすき間で並び直る。
  ゲストには、`vim` と `nano` がある。
- スクリプトが壊れていると、画面は fallback に切り替わる。ウィンドウは見えたままで、クリックと文字入力ができる。
  何が失敗したかは、制御口の `state` の、`script` の `mode`（`running` か `safe`）と、`last_failures` にある。
- 制御口からも操作できる。`script-reload`（読み直し）、`script-mode safe`（fallback にする）、`script-call`（スクリプトの関数を呼ぶ）。

プロジェクトの置き場所（ゲスト）:

| 場所 | 内容 |
| --- | --- |
| `~/hzk-scripts.orig/` | `crates/hizukuri/scripts/` の写し。`tools/vm/try-hizukuri` が、毎回置き換える |
| `~/hzk-scripts/` | `hzk script` が使うもの。無ければ、上の写しから作る。**ゲストで書き換えるのは、ここである** |
| `~/hzk-scripts.bak/` | `hzk script --reset-scripts` が、書き換えたものを退避する場所 |

どれも、VM を止めると消える。残したい変更は、`tools/vm/vm pull` で取り出す。

### 画面なしで操作する

```sh
nix develop --command tools/vm/try-hizukuri    # ビルド、起動（画面なし）、送付

# hzk で起動する（端末が 3 つ並ぶ）
tools/vm/vm tty-run -- hzk --seconds 120
tools/vm/vm tty-run -- hzk script --seconds 120

# compositor を、直接起動する
tools/vm/vm tty-run -- /home/hizukuri/bin/hizukuri --backend kms --vulkan-device cpu --exit-after 60

tools/vm/vm screendump frame.png               # 画面を取り込む
tools/vm/vm pixels 100,100 640,400             # 画素の値を見る
tools/vm/vm move 640 400 && tools/vm/vm click  # 入力を送る
tools/vm/vm type 'hello'
tools/vm/vm key ctrl-alt-f2                    # VT の切り替えを、compositor に処理させる
tools/vm/vm ssh '~/bin/hizukuri-ctl --socket /run/user/1000/hizukuri-0.control state'
tools/vm/vm tty-log                            # ログを読む
tools/vm/vm tty-stop                           # 止める
tools/vm/vm stop
```

制御口の socket は、`hizukuri-0.control` である。`--socket NAME` を付けて直接起動したときは、`NAME.control` になる。
起動時の指定は、[検証用の compositor](../prototype/README.md)にある。

## VM の中の GPU と KMS

VM の中で compositor を動かすときに、前提になることである。

- **virtio-gpu の primary plane の format は、`XRGB8888` だけである。modifier を受け取らない。**
  `ARGB8888` は、cursor plane だけが持つ。
- virtio-gpu の plane は、primary と cursor だけである。HDR、VRR、async page flip は、無い。
- **scanout は、dumb buffer へ、CPU で写す。** lavapipe で描いたフレームを読み戻して、写す。
- **スクリプトの描画は、lavapipe で動かす。** lavapipe は、`VK_EXT_descriptor_heap` を持つ。
  `--vulkan-device cpu` で、lavapipe を選ぶ。
- VT を離れるとき、session の一時停止の通知より先に、page flip が `EACCES` で失敗する。
- ゲストの session には、`VK_DRIVER_FILES` が設定されている。実行ファイルに file capabilities が付いていると、
  Vulkan の loader は、この変数を無視する。

### Venus

```sh
HIZUKURI_VM_GPU=venus tools/vm/vm start
```

VM を動かす機械に GPU があれば、ゲストに、その GPU で描く Vulkan のデバイス（Venus）を見せられる。
表示のデバイスは `virtio-vga-gl`（`blob=true,venus=true`）になり、QEMU は、`egl-headless`（`--gtk` なら `gtk,gl=on`）で表示する。

- `guest.nix` の QEMU は、同じ nixpkgs の Mesa で、GPU を使う。Mesa にドライバーがある GPU が要る。
  それ以外の GPU では、`HIZUKURI_VM_QEMU` で、その機械の QEMU を指定する。
- **ゲストに、Vulkan のデバイスが、複数見える。** Venus のデバイスと、lavapipe である。
  先頭のデバイスを選ぶ作りにしない。

| | Venus | lavapipe |
| --- | --- | --- |
| `VK_EXT_descriptor_heap` | 無い。virglrenderer が、この拡張をゲストへ通す必要があり、1.3.0 は通さない | ある |
| 描けるもの | Rust のレンダラー（`hizukuri-render`） | Rust のレンダラーと、スクリプトの描画（`gfx-core`） |
| scanout | Vulkan で確保した `DRM_FORMAT_MOD_LINEAR` の画像を、dma-buf として取り出して、flip する | dumb buffer へ、CPU で写す |

- scanout する画像は、`DRM_FORMAT_MOD_LINEAR` で確保し、`drmModeAddFB2` に、modifier を付けない。
  ドライバーに modifier を選ばせると、virtio-gpu の KMS が扱えない modifier が選ばれうる。
- GBM で確保したバッファを、Venus に取り込む経路は、使えない。ゲストの Venus のドライバーが、abort する。
- virtio-gpu の plane は、`IN_FENCE_FD` を持つ。描画の fence から sync file を取り出して、渡せる。
- Venus で描くクライアントの dma-buf は、compositor の側で、mmap では読めない。

## 複数の出力と hotplug

virtio-gpu の出力は、既定で 2 つある。

```sh
tools/vm/vm output 1 on                     # 2 つ目の出力をつなぐ（ゲストの Virtual-2 が、connected になる）
tools/vm/vm pixels --head 1 100,100         # 2 つ目の出力の画面を読む
tools/vm/vm output 1 off                    # 外す
tools/vm/vm output 0 off                    # 1 つ目の出力も、外せる
```

- 2 つ目以降の出力は、未接続で始まる。QEMU の virtio-gpu は、表示がつながっていない出力を、ゲストに未接続と伝える。
  `vm output N on` は、その出力の VNC に、大きさを頼む（RFB の SetDesktopSize）。
  QEMU が、それを virtio-gpu に伝え、ゲストに hotplug のイベントが届く。`off` は、大きさ 0 を頼む。
- 出力ごとに、VNC の Unix socket がある（`run/vnc.sock`、`run/vnc-1.sock`）。`--head N` で、読む出力を選ぶ。
- 頼める大きさは、`HIZUKURI_VM_XRES` と `HIZUKURI_VM_YRES` までである。
- ゲストのコンソール（fbcon）は、つながっている出力のすべてに出る。compositor が使っていない出力には、それが残る。
- 出力の数は、`HIZUKURI_VM_OUTPUTS` で変える。

## ゲストを、仮想化されていない機械に見せる

```sh
HIZUKURI_VM_CPU=host,-hypervisor tools/vm/vm start
```

CPU の `hypervisor` フラグを隠して起動する。ゲストの `/proc/cpuinfo` から、`hypervisor` が消える。
検証用の compositor は、このフラグで、VM の中かどうかを判定する。この構成では、ゲストを、仮想化されていない機械として扱う。
VM の外で起動するときの安全策（`--allow-real-hardware`、`--kms-device`、`--kms-output` の 3 つがそろわないと、起動しない）を、
VM の中で試験するために使う。

`systemd-detect-virt` は、DMI の情報から、引き続き `qemu` と答える。

## メインループを固まらせる

```sh
tools/vm/vm ssh 'freeze-main-loop PID watchdog 30'
```

gdb で、`watchdog` という名前のスレッドだけを動かし、ほかのスレッド（メインスレッドを含む）を止めたままにする。
watchdog が、固まった compositor を終わらせることを、確かめるために使う。

- 対象は、PID で指定する。`tty-run` で起動したものなら、`tools/vm/vm tty-status` の `MainPID` である。
- プロセスが終わるか、指定した秒数が過ぎるまで待つ。`gone` か `alive` と、かかったミリ秒を出す。

## vkms を使う（VT を使わない）

ゲストの中で、vkms（仮想の KMS のドライバー）をロードすると、virtio-gpu とは別の、DRM のデバイスが増える。

```sh
tools/vm/vm ssh vkms-load                              # ロードして、デバイスの場所を表示する
tools/vm/vm ssh '~/bin/hizukuri kms-info --summary'    # デバイスと、connector の名前を見る
```

- **VT も seat も、要らない。** `video` グループの利用者が、デバイスを直接開いて、DRM master になれる。
  SSH から、そのまま、KMS の試験を動かせる。
- writeback connector で、scanout の結果を取り出せる。
- overlay plane が、8 枚ある。16 bit の format、YUV の format、`GAMMA_LUT`、plane の color pipeline を持つ。
- GPU を使わない。描画は、lavapipe で行う。
- connector は、probe するまで、状態もモードも持たない。probe ありで取得する。

検証用の compositor も、vkms の上で、VT なしで動く。libseat の `noop` バックエンドを使う（デバイスを直接開く。VT の切り替えは、無い）。
`cardN` と `Virtual-N` には、上の 2 つのコマンドが出した名前を入れる。

```sh
tools/vm/vm ssh 'XDG_RUNTIME_DIR=/run/user/1000 LIBSEAT_BACKEND=noop ~/bin/hizukuri --backend kms \
    --kms-device /dev/dri/cardN --vulkan-device cpu --exit-after 30'
tools/vm/vm ssh 'XDG_RUNTIME_DIR=/run/user/1000 LIBSEAT_BACKEND=noop ~/bin/hizukuri kms-selftest \
    --device /dev/dri/cardN --output Virtual-N --seconds 5 --report /tmp/report --vulkan-device cpu --scanout cpu'
```

検証用の compositor は、writeback connector を使っていない。vkms が表示した画素の検査は、未実装である。

## 自動の試験

```sh
tools/vm/vm test [--keep] SCRIPT [ARGS...]
```

`vm test` は、次を順に行う。

1. VM が止まっていれば、ゲストをビルドして、起動する。SSH を待つ。
2. SCRIPT を実行する。SCRIPT は、環境変数 `HIZUKURI_VM` にある `vm` のコマンドを使う。
3. 自分で起動した VM なら、止める（`--keep` で、動かしたままにできる）。
4. SCRIPT の終了コードを返す。

VM が動いていれば、そのまま使う。その VM の構成（GPU、CPU の model）は、変わらない。

### compositor の試験（`tools/vm-check.py`）

```sh
nix develop --command tools/vm/vm test tools/vm-check.py
```

devShell の中で実行する。検証用の compositor をビルドし、ゲストへ送り、DRM/KMS の session として動かして確かめる。
GPU は、要らない。数分で終わる。

| 指定 | 内容 |
| --- | --- |
| `--only PART,...` | 回す部を選ぶ。既定は、`lavapipe,script,vkms,safety,outputs` |
| `--out DIR` | 結果を置く場所。既定は、`artifacts/vm-check/<日時>/` |
| `--skip-build` | ビルドしない。すでにあるバイナリを送る |

| 部 | 内容 | VM の構成 |
| --- | --- | --- |
| `lavapipe` | Rust のレンダラーを lavapipe で動かし、dumb buffer で scanout する。起動、モード、ウィンドウ、ポインター、キー、VT の切り替え、実際のアプリ、終了のしかた（キー、時間、制御口、SIGHUP）、`hzk`、プロトコル、スクリプトの層、自己診断（`kms-selftest`） | 既定 |
| `script` | スクリプトの層を、実際のアプリで。タイル、キーの割り当て、スクリプトの故障と読み直し、VT の切り替え、watchdog、Rust のレンダラーとの画素の比較、フレームの時間、`hzk script` | 既定 |
| `vkms` | vkms の上で、VT なしで起動する。自己診断 | 既定 |
| `safety` | watchdog（メインループが固まると終わること、プロセスの全体が止められただけでは終わらないこと） | 既定 |
| `outputs` | 2 つの出力。使う出力の指定、ほかの出力に触れないこと、hotplug | 既定 |
| `real-hardware` | VM の外で起動するときの安全策 | `HIZUKURI_VM_CPU=host,-hypervisor` |
| `venus` | `lavapipe` と同じ項目を、Venus と dma-buf で（スクリプトの層を除く） | `HIZUKURI_VM_GPU=venus` |

既定の実行に入らない 2 つの部は、VM の起動のしかたが違う。VM を止めてから、実行する。

```sh
HIZUKURI_VM_CPU=host,-hypervisor nix develop --command tools/vm/vm test tools/vm-check.py --only real-hardware
HIZUKURI_VM_GPU=venus nix develop --command tools/vm/vm test tools/vm-check.py --only venus
```

- `venus` の部は、GPU のある機械で、明示して回す。
- **構成が合わない VM に、部を頼むと、失敗になる**（`<部>/configured` が FAIL）。飛ばさない。
- 画面は、VNC の socket から読み、数値で比べる。入力は、QMP から送る。
- 試験が起動した、ゲストの中のプロセスは、起動したときの PID と、`tty-run` の unit の `MainPID` で扱う。名前で探して止めることは、しない。
- 送ったバイナリが、ゲストで起動しないときは、devShell でビルドするよう出して、止まる。

### 結果の読み方

項目ごとに、1 行が出る。部が終わるたびに、その部の時間が出る。最後に、合計が出る。

```
PASS lavapipe/vt-switch: tty7 to tty2 and back: paused, resumed, 1 windows kept, pixels and input are back
TIME lavapipe: <秒> s
vm-check: <数> passed, <数> failed, <数> skipped. Evidence: <出力のディレクトリ>
```

- 各行は、`PASS`、`FAIL`、`SKIP` のどれかと、`<部>/<項目>`、確かめた内容である。
- 終了コードは、FAIL が 1 つでもあれば 1、無ければ 0 である。
- 時間に関わる検査（page flip の遅れ、フレームの時間）は、遅い機械では、行の中の `[pass]` が `[warn]` になる。項目は、失敗にならない。

出力のディレクトリに、次のものが残る。

| ファイル | 内容 |
| --- | --- |
| `summary.txt`、`results.json` | 全部の項目と、その内容。部ごとの時間 |
| `guest.txt` | ゲストの kernel、NixOS の版、構成の store のパス、Vulkan のデバイス |
| `kms-info.json`、`kms-summary.txt` | ゲストの DRM のデバイスの記述（`hizukuri kms-info`） |
| `<部>/hizukuri-<段階>.log` | compositor のログ。失敗した項目のものは、`hizukuri-fail-<項目>.log` |
| `<部>/state-<段階>.json` | compositor の状態（制御口の `state`） |
| `<部>/screen-<段階>.png` | 取り込んだ画面 |
| `<部>/selftest*/` | `kms-selftest` の報告 |
| `script/frame-time-apps-1080p.json` | Rust のレンダラーと、スクリプトのレンダラーの、フレームの時間 |

失敗を調べるときは、`--keep` で VM を残し、`tools/vm/vm tty-log`、`tools/vm/vm screendump`、`tools/vm/vm ssh` で、ゲストを見る。
ゲストの起動の失敗は、`HIZUKURI_VM_DIR` の `log/serial.log` に出る。

### CI

`.github/workflows/vm.yml` が、GitHub の runner（KVM が使える。GPU は無い）の上で、同じ試験を回す。

- 契機は、`main` への push（文書だけの変更を除く）、毎晩 1 回、手動の起動である。
- 既定の部と、`real-hardware` の部を回す。`venus` の部は、回さない。
- cargo のビルドの結果を、キャッシュする。無くても、時間がかかるだけである。Nix の store は、キャッシュしない（devShell、QEMU、ゲストは、binary cache から、1 分ほどで取得できる）。
- 結果の要約は、job の summary に出る。出力のディレクトリと、ゲストのシリアルコンソールのログは、artifact（`vm-check`）に残る。

CI の全体は、[テストと CI](testing-and-ci.md)にある。

## 参照用のデータ

[`tools/vm/reference/`](../../tools/vm/reference/) に、VM の中の DRM のデバイスから取った記録がある。模擬の DRM のデバイスの入力に使える。

| ファイル | 内容 |
| --- | --- |
| `virtio-gpu-drm_info-<kernel>.json`、`.txt` | virtio-gpu の KMS の、connector、CRTC、plane、property の一覧（`drm_info -j`） |
| `virtio-gpu-edid-1280x800.bin` | virtio-gpu の connector の EDID |
| `vkms-drm_info-<kernel>.json`、`.txt` | vkms の一覧 |

## VM で確かめられること、確かめられないこと

| 確かめること | 使うもの | `vm-check.py` の部 |
| --- | --- | --- |
| libseat（logind）で、session を取る | virtio-gpu、`tty-run` | `lavapipe` |
| atomic commit でのモード設定、page flip、flip のイベント | virtio-gpu、vkms | `lavapipe`、`vkms` |
| VT の切り替え（session の一時停止と再開） | virtio-gpu、QMP の `ctrl-alt-fN` | `lavapipe`、`script` |
| libinput でのキー、ポインター、ボタン | QMP | `lavapipe` |
| Vulkan 1.4 と descriptor heap での描画を、KMS に出す | lavapipe、dumb buffer | `lavapipe`、`script` |
| 出した画面の、画素の値 | VNC（virtio-gpu） | `lavapipe`、`script`、`outputs` |
| 複数の出力、hotplug | virtio-gpu の 2 つ目の出力、`vm output` | `outputs` |
| メインループが固まったときの、watchdog の動作 | `freeze-main-loop` | `safety`、`script` |
| VM の外で起動するときの安全策 | CPU の `hypervisor` フラグを隠した構成 | `real-hardware` |
| dma-buf を、KMS に載せる経路 | Venus（`LINEAR`、`XRGB8888`） | `venus` |
| libseat の seatd のバックエンド | `tty-run --seatd` | 試験は、未実装 |
| ジェスチャー、タッチパッド、入力の機器の抜き差し | uinput（`uinput-touchpad.py`）、`libinput replay` | 試験は、未実装 |
| vkms が表示した画素の値 | vkms の writeback | 試験は、未実装 |
| overlay plane と cursor plane への割り当て、16 bit の framebuffer、`GAMMA_LUT`、plane の color pipeline | vkms | 試験は、未実装 |

次のものは、VM では確かめられない。

- HDR の出力と、10 bit 以上の scanout。
- VRR と、ティアリング（async page flip）。
- 実ドライバーの modifier（tiled のバッファの scanout）、plane の format と scaling の制約。
- GPU のドライバーでの、descriptor heap を使う描画から、KMS までの経路と、その性能。
- 表示の実際のタイミング（vblank、presentation の時刻）。
- モニターごとに違う EDID、複数の GPU。
