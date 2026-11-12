# xwayland-satellite を動かすために必要なもの

- 確認した版: xwayland-satellite 0.8.3（`b424e5e44f1f`、2026-11-05）、Smithay（`8fd9fe0c74e5`）

hizukuri は、X11 のアプリを、xwayland-satellite に任せる。この文書は、xwayland-satellite が compositor に求めるものと、
compositor から起動するときの取り決めをまとめる。X11 の socket の用意と、xwayland-satellite の起動は、未実装である。

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
- compositor は、X11 のことを、何も知らなくてよい。

## 2. compositor に必要なプロトコル

### 必須

xwayland-satellite の README と、ソース（`src/server/mod.rs`）が bind する版の範囲である。

| interface | xwayland-satellite が受け付ける版 | Smithay が公開する版 |
| --- | --- | --- |
| `wl_compositor` | 4〜6 | 6 |
| `wl_subcompositor` | 1 | 1 |
| `wl_shm` | 1 | 2 |
| `xdg_wm_base` | 2〜6 | 7（クライアントは、6 までで bind する） |
| `wp_viewporter` | 1 | 1 |
| `wl_output`、`wl_seat` など、core の interface | ― | あり |

viewporter は、拡大縮小のために必須である。

### あれば使うもの

README が挙げているものである。

linux-dmabuf、xdg-activation、xdg-decoration、xdg-foreign、pointer-constraints、primary-selection、tablet、fractional-scale。

## 3. compositor から起動する

xwayland-satellite は、Xwayland の `-listenfd` の引数を、そのまま Xwayland に渡す。
compositor が X11 の socket を作って待ち、接続が来たときに、xwayland-satellite を起動できる。

### 起動の引数

- 最初の引数に、display の番号（`:<番号>`）を渡す。続けて、socket の fd ごとに、`-listenfd <fd>` を渡す。
- `xwayland-satellite :0 --test-listenfd-support` で、`-listenfd` に対応した版かを確かめられる。
- xwayland-satellite は、Xwayland を、`-rootless` などの引数で起動する（`src/lib.rs`）。Xwayland は、23.1 以上が要る。

### 流れ

1. **compositor が、X11 の socket を作る。**
   - 使われていない display の番号を選ぶ。
   - `/tmp/.X11-unix/X<番号>` の Unix socket と、同じ名前の abstract socket を作る。
   - lock のファイル（`/tmp/.X<番号>-lock`）を作り、自分の PID を書く。作れなければ、次の番号を試す。
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
  次の起動で、古いものを見分けて片付ける必要がある。
- **`DISPLAY` の範囲。** compositor より先に起動したプロセス（systemd の user のサービスなど）には、`DISPLAY` が伝わらない。
  session として動かすときは、環境変数を、systemd と D-Bus に知らせる。
- **display の番号の衝突。** `/tmp/.X11-unix/` は、機械の全体で共有である。専用の `XDG_RUNTIME_DIR` を使っても、分かれない。
  デスクトップが動いている機械で検証するときは、デスクトップが使っている番号と、重ならない番号を選ぶ。
- xwayland-satellite を更新したら、compositor の再起動が要る（README）。
- xwayland-satellite の出力は、compositor の出力に混ぜない。

## 4. compositor から見た X11 のウィンドウ

- X11 のウィンドウは、xwayland-satellite という 1 つのクライアントの、複数の toplevel として見える。
- `app_id` とタイトルは、xwayland-satellite が、X11 の property から設定する。
- compositor は、X11 のウィンドウを、ほかのウィンドウと同じように配置できる。
- X11 のアプリは、color-management を使わない。表面は、色の指定が無いものとして届く。
- **画面上の決まった位置に、自分でウィンドウやバーを置こうとする X11 のアプリは、正しく動かない。**
  Wayland には、クライアントが、絶対的な位置を指定する仕組みが無い。

## 5. 出典

確認日は 2026-11-12。

- xwayland-satellite（`b424e5e44f1f`）: `README.md`、`src/server/mod.rs`、`src/lib.rs`、`src/main.rs`
  https://github.com/Supreeeme/xwayland-satellite
- Smithay（`8fd9fe0c74e5`）: 各 global の版
  https://github.com/Smithay/smithay
