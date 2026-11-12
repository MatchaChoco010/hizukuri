# xwayland-satellite を動かすために必要なもの

- 確認した版: xwayland-satellite 0.8.3（`b424e5e44f1f`、2026-11-05）、Xwayland 24.1.14、Smithay（`8fd9fe0c74e5`）

hizukuri は、X11 のアプリを、xwayland-satellite に任せる。この文書は、xwayland-satellite が compositor に求めるものと、
compositor から起動するときの取り決めと、X11 のウィンドウが compositor からどう見えるかをまとめる。
検証用の compositor での作りは、[検証用の compositor](../../prototype/README.md#x11-のアプリ)にある。

## 1. 仕組み

X11 のアプリを Wayland の compositor で動かすには、ふつう、compositor 自身が、X11 の window manager になる
（Xwayland の rootless の方式）。xwayland-satellite は、その役を、別のプロセスとして引き受ける。

```text
X11 のアプリ ── X11 ── Xwayland ── Wayland ── xwayland-satellite ── Wayland ── compositor
                                   （satellite が compositor の役）   （satellite はふつうのクライアント）
```

- xwayland-satellite は、Xwayland に対しては、Wayland の compositor と、X11 の window manager として振る舞う。
- compositor に対しては、**ふつうの Wayland のクライアント**として振る舞う。
  X11 のウィンドウを、xdg-shell の toplevel と popup にして見せる。
- compositor は、X11 のプロトコルを話さない。
- Xwayland の Wayland の要求の多くは、xwayland-satellite が、そのまま compositor へ通す（下の表）。
  compositor から見ると、Xwayland の要求も、xwayland-satellite の接続から来る。

## 2. compositor に必要なプロトコル

### 必須

xwayland-satellite は、起動時に、次の global を bind する。どれかが無いと、panic して終わる（`src/server/mod.rs`）。

| interface | xwayland-satellite が受け付ける版 | Smithay が公開する版 |
| --- | --- | --- |
| `xdg_wm_base` | 2〜6（2 では popup の置き直しができない） | 7 |
| `wl_compositor` | 4〜6 | 6 |
| `wl_subcompositor` | 1 | 1 |
| `wl_shm` | 1 | 2 |
| `wp_viewporter` | 1 | 1 |

`wl_seat`、`wl_output` も、無ければ入力と画面の情報が届かない。

### あれば使うもの

xwayland-satellite が、自分で使うもの。無ければ、起動のときにログを出して、その機能なしで動く。

| interface | 用途 | 無いとき |
| --- | --- | --- |
| `wp_fractional_scale_manager_v1` | 出力の倍率を知り、X11 のアプリを、その倍率の画素で描かせる | warn。整数の倍率だけになる |
| `xdg_activation_v1` | X11 のアプリが、ウィンドウの activate を求めたとき（`_NET_ACTIVE_WINDOW`）と、`XDG_ACTIVATION_TOKEN` を持って起動したアプリのウィンドウ | warn。activate の要求は、compositor に届かない |
| `zxdg_decoration_manager_v1` | 装飾を compositor に任せる | X11 のアプリが装飾を compositor に求めるウィンドウに、xwayland-satellite が自分で題名の帯を描く |
| `wl_data_device_manager` | X11 の CLIPBOARD と、Wayland の selection を、つなぐ | warn。コピーと貼り付けが、X11 と Wayland の間で通らない |
| `zwp_primary_selection_device_manager_v1` | X11 の PRIMARY と、Wayland の primary selection を、つなぐ | info。PRIMARY が、X11 と Wayland の間で通らない |

README は xdg-foreign も挙げているが、0.8.3 のソースは、xdg-foreign を bind しない。

### Xwayland へ、そのまま通すもの

compositor が公開していれば、xwayland-satellite が、同じ interface を Xwayland に公開し、要求を通す
（`handle_new_globals`）。版は、compositor の版と、xwayland-satellite が知る版の、小さいほうである。

`wl_compositor`、`wl_shm`、`wl_seat`、`wl_output`、`zxdg_output_manager_v1`、`zwp_linux_dmabuf_v1`、`wl_drm`、
`zwp_relative_pointer_manager_v1`、`zwp_pointer_constraints_v1`、`zwp_tablet_manager_v2`、`wp_linux_drm_syncobj_manager_v1`。

- pointer の lock と confine、relative motion は、Xwayland が、これらを使って行う。xwayland-satellite は、間に立つだけである。
- `zwp_linux_dmabuf_v1` が無くても、X11 のアプリは表示される（Xwayland は、`wl_shm` で描く）。

## 3. compositor から起動する

xwayland-satellite は、Xwayland の `-listenfd` の引数を、そのまま Xwayland に渡す。
compositor が X11 の socket を作って待ち、接続が来たときに、xwayland-satellite を起動できる。

### 起動の引数と環境

- 最初の引数に、display の番号（`:<番号>`）を渡す。続けて、socket の fd ごとに、`-listenfd <fd>` を渡す。
  fd は、close-on-exec を外して、子に引き継ぐ。
- `xwayland-satellite :0 --test-listenfd-support` で、`-listenfd` に対応した版かを確かめられる。
- xwayland-satellite は、Xwayland を、`-rootless`、`-force-xrandr-emulation`、`-wm <fd>`、`-displayfd <fd>` で起動する
  （`src/lib.rs`）。Xwayland は、23.1 以上が要る。
- **compositor への接続は、`WAYLAND_DISPLAY` で渡す。`WAYLAND_SOCKET` は使えない。** xwayland-satellite は、compositor に
  つなぐ前に、Xwayland を、自分の環境を引き継いで起動する（`src/lib.rs` の `main`）。`WAYLAND_SOCKET` があると、Xwayland が
  それを見て、compositor の接続の fd を、xwayland-satellite と取り合う。
  そのため、compositor は、xwayland-satellite の接続を、渡した socket では見分けられない。接続の相手のプロセスの id
  （`SO_PEERCRED`）で見分ける。
- xwayland-satellite は、自分が Xwayland のための Wayland の socket を、`XDG_RUNTIME_DIR` に作る（`xwls-<番号>`）。
- Xwayland が終わると、xwayland-satellite も終わる。compositor との接続が切れると、xwayland-satellite は panic して終わる。

### 流れ

1. **compositor が、X11 の socket を作る。**
   - 使われていない display の番号を選ぶ。
   - lock のファイル（`/tmp/.X<番号>-lock`）を、排他的に作り、自分の PID を書く（右寄せで 10 文字と改行）。
     作れなければ、次の番号を試す。ファイルがあっても、書かれた PID のプロセスが無ければ、古いものとして消してよい。
   - `/tmp/.X11-unix/X<番号>` の Unix socket と、同じ名前の abstract socket を作る。`/tmp/.X11-unix` が無ければ、
     mode `1777` で作る。
2. **compositor が、`DISPLAY` を公開する。** 自分が起動するプロセスの環境変数に、`DISPLAY=:<番号>` を入れる。
3. **compositor が、socket を見張る。** イベントループに、socket を登録する。
4. **X11 のアプリが、socket につないでくる。**
5. **compositor が、xwayland-satellite を起動する。**
   起動している間、compositor は、socket の見張りをやめる。接続は、Xwayland が受け取る。
6. **xwayland-satellite が、compositor の Wayland の socket につないでくる。** ふつうのクライアントである。
7. **xwayland-satellite が終了したら、** compositor は、socket の見張りを再開する。次の接続で、起動し直す。

### 気を付けること

- **起動に失敗したとき。** socket に、接続が残ったままになる。そのまま見張りを再開すると、すぐにまた起動を試みて、繰り返しになる。
  見張りを再開する前に、残っている接続を、すべて受け取って捨てる。
- **socket の後始末。** compositor が異常終了すると、`/tmp/.X11-unix/` の socket と lock が残る。
  次の起動で、lock の PID から古いものを見分けて、片付ける。
- **`DISPLAY` の範囲。** compositor より先に起動したプロセス（systemd の user のサービスなど）には、`DISPLAY` が伝わらない。
  session として動かすときは、環境変数を、systemd と D-Bus に知らせる。
- **display の番号の衝突。** `/tmp/.X11-unix/` と abstract socket の名前空間は、機械の全体で共有である。専用の `XDG_RUNTIME_DIR` を使っても、分かれない。
  デスクトップが動いている機械で検証するときは、デスクトップが使っている番号と、重ならない番号を選ぶ。
- xwayland-satellite を更新したら、compositor の再起動が要る（README）。
- xwayland-satellite の出力は、compositor の出力に混ぜない。

## 4. compositor から見た X11 のウィンドウ

X11 のウィンドウは、xwayland-satellite という 1 つのクライアントの、複数の surface として来る。
xwayland-satellite が、X11 のウィンドウの種類を、次のように振り分ける（`create_role_window`、`window_role_heuristics`）。

| X11 のウィンドウ | Wayland での形 |
| --- | --- |
| ふつうのウィンドウ | `xdg_toplevel`。`set_title` は `_NET_WM_NAME`（無ければ `WM_NAME`）、`set_app_id` は `WM_CLASS` の class |
| `WM_TRANSIENT_FOR` のあるウィンドウ（ダイアログ） | `xdg_toplevel`。最初の commit の前に、親のウィンドウの toplevel で `set_parent` |
| サイズの hint（`WM_NORMAL_HINTS` の最小と最大） | `set_min_size`、`set_max_size`。splash screen（`_NET_WM_WINDOW_TYPE_SPLASH`）は、最小と最大を、いまの大きさにする |
| 大きさが、ある出力と同じウィンドウ（map の時点） | 最初から `set_fullscreen` した `xdg_toplevel` |
| `_NET_WM_STATE_FULLSCREEN` の要求 | `set_fullscreen`、`unset_fullscreen` |
| `_NET_ACTIVE_WINDOW` の要求 | `xdg_activation_v1` の token（最後のキーボードの serial と、最後に focus のあった toplevel の surface を付ける）で `activate` |
| `_NET_WM_MOVERESIZE` の要求 | `xdg_toplevel.move` |
| override-redirect のウィンドウ（メニュー、tooltip）、popup と判定されたウィンドウ | `xdg_popup`（下） |

popup の作り方:

- 親は、最後に pointer が入った toplevel、無ければ最後に focus のあった toplevel である。どちらも無ければ、toplevel にする。
- positioner は、大きさがウィンドウの大きさ、anchor の矩形が親の全体、anchor と gravity が左上と右下、offset が
  2 つの X11 のウィンドウの位置の差である。constraint adjustment は、x と y の slide だけを許す。
- **`xdg_popup.grab` を使わない。** メニューの grab は、X server の中で行われる。メニューの外を押しても、
  compositor が `popup_done` を送ることは無い。
- X11 のウィンドウが動くと、`xdg_popup.reposition`（版 3 以上）で置き直す。

そのほかの振る舞い:

- X11 の座標では、toplevel は、どれも、出力の左上に置かれる。pointer が入った toplevel が、X11 の重なりの最上になる。
  そのため、**画面上の決まった位置に、自分でウィンドウやバーを置こうとする X11 のアプリは、正しく動かない。**
- 装飾は、compositor が decoration に答えれば、X11 のアプリの求めにかかわらず、xwayland-satellite は描かない。
- X11 のアプリは、color-management を使わない。表面は、色の指定が無いものとして届く。
- **倍率。** xwayland-satellite は、出力の倍率のうち最も小さいものを、X11 の全体の倍率にする。X11 のアプリには、
  物理的な画素の大きさで configure し、`Xft/DPI` などを XSETTINGS で倍率に合わせ、`wp_viewport.set_destination` で
  論理的な大きさを compositor に伝える。fractional-scale があれば、倍率が 1.5 の出力で、論理の 1.5 倍の大きさのバッファが
  届く（ぼやけない）。倍率の違う出力が並ぶと、大きい倍率の出力では、文字が小さく見える。
- compositor は、どのウィンドウが X11 のものかを、プロトコルでは知らされない。知る手段は、クライアントが
  xwayland-satellite であること（接続の相手のプロセス）である。

## 5. 出典

確認日は 2026-11-12。

- xwayland-satellite（`b424e5e44f1f`、0.8.3）: `README.md`、`ARCHITECTURE.md`、`src/lib.rs`、`src/main.rs`、
  `src/server/mod.rs`、`src/server/selection.rs`、`src/xstate/mod.rs`、`src/xstate/settings.rs`
  https://github.com/Supreeeme/xwayland-satellite
- Smithay（`8fd9fe0c74e5`）: 各 global の版
  https://github.com/Smithay/smithay
- 振る舞い（popup の位置、ダイアログの親、selection、倍率、xwayland-satellite が終わったときの compositor）は、
  検証用の VM の中で、nixpkgs の xwayland-satellite 0.8.3 と Xwayland 24.1.14 で確かめた（`tools/vm-check.py` の `x11` の部）。
