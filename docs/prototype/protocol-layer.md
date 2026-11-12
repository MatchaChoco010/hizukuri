# プロトコルの層

`crates/hizukuri-wl` は、Wayland のサーバー側である。Smithay のプロトコルの実装を包み、プロトコルの要求を、
ふつうの状態と出来事に直す。公開する型に、Smithay の型は出てこない。表面、ウィンドウ、出力は id で表し、
画素はバイト列で渡し、入力は数値で受け取る。描画もしないし、ウィンドウをどこに置くかも決めない。

## 呼び出す側との関係

呼び出す側（中核）は、次のように使う。

1. `WlServer::listen_fd` と `WlServer::display_fd` を見張り、readable になったら `accept_clients` と `dispatch_clients` を呼ぶ。
   `display_fd` は、クライアントから要求が届いたときと、待っていたバッファが読めるようになったときに、readable になる。
2. `drain_events` で出来事を取り出して、応じる（ウィンドウを置く、focus を決める、configure を送る）。
3. ウィンドウごとに、表面の一覧（`window_surfaces`）と、commit されたバッファ（`take_buffer_update`）を取り出して描く。
4. フレームの完了を伝え（`send_frame_callbacks`、`send_presented`）、`flush_clients` を呼ぶ。

ポインターの入力は、表面と、その中の座標で渡す（`pointer_motion`）。当たり判定は、呼び出す側の仕事である。
ウィンドウを平らに並べても、変形して描いても、同じ口で届く。

出来事（`WlEvent`）:

| 出来事 | 内容 |
| --- | --- |
| `WindowCreated`、`WindowChanged`、`WindowDestroyed` | toplevel ができた、その木のどこかが commit した、無くなった |
| `SurfaceDestroyed` | 表面が無くなった。表面のために持っている資源を捨ててよい |
| `CursorChanged` | ポインターの下のクライアントが求めるカーソルが変わった |
| `WindowRequest { window, request }` | クライアントが、ウィンドウ管理の決めることを求めた（下） |
| `PopupPlacement { window, popup }` | popup ができた、または置き直しを求められた（下） |

## 公開しているプロトコル

| global | 版 | 補足 |
| --- | --- | --- |
| `wl_compositor` | 6 | |
| `wl_subcompositor` | 1 | |
| `wl_shm` | 2 | `argb8888`、`xrgb8888`、`abgr8888`、`xbgr8888` |
| `wl_seat` | 9 | キーボードとポインター。タッチは未実装 |
| `wl_output` | 4 | 出力ごとに 1 つ |
| `wl_data_device_manager` | 3 | selection と drag and drop |
| `xdg_wm_base` | 7 | |
| `zxdg_output_manager_v1` | 3 | |
| `zxdg_decoration_manager_v1` | 1 | どの要求にも、server side と答える |
| `wp_viewporter` | 1 | |
| `wp_presentation` | 2 | |
| `wp_fractional_scale_manager_v1` | 1 | |
| `wp_cursor_shape_manager_v1` | 2 | |
| `zwp_linux_dmabuf_v1` | 6 または 3 | `--dmabuf` で公開したときだけ（下） |

未実装: `wp_color_management_v1`、`zwp_relative_pointer_manager_v1`、`zwp_pointer_constraints_v1`、
primary selection、data-control、text-input と input-method、タッチとジェスチャー。

## バッファ

`take_buffer_update` が返す `BufferUpdate` が、バッファを受け渡す口である。

```rust
pub struct BufferUpdate {
    pub source: BufferSource,        // Shm(画素) か Dmabuf(記述)
    pub transform: BufferTransform,  // バッファの中での向き
    pub damage: Vec<DamageRect>,     // クライアントが付けた damage（バッファの画素）
    // バッファへの参照を持つ。drop すると、クライアントに release を送る
}
```

- `wl_shm` の画素は、commit のときではなく、取り出すときに読む。表示される前に差し替えられたバッファは、
  読まずに release する。
- **`BufferUpdate` を持っている間、クライアントは、そのバッファを再利用できない。** 画素を複写した側は、
  すぐ手放してよい。dmabuf を取り込んで使う側は、使っている間、持ち続ける。
- `BufferUpdate::upright_pixels()` は、内容を、正しい向きの画素として CPU のメモリに返す。`wl_shm` は並べ替えるだけで、
  dmabuf は mmap して複写する。dmabuf を取り込めない描画（Rust のレンダラー）のための、遅い経路である。

### buffer transform

`wl_surface.set_buffer_transform` の 8 通りを扱う。`SurfaceNode` の `size`（表面の大きさ）と `source`（表示する範囲）は、
正しい向きに直した後の画像についての値である。`buffer_size` は、クライアントが確保したままの大きさである。
dmabuf を GPU に取り込む描画は、`BufferUpdate::transform` を見て、自分で向きを直す。

transform だけを変えて、バッファを付け直さない commit は、表示に反映されない。

### dmabuf

`--dmabuf` で、クライアントから dmabuf を受け取るかどうかを決める。

| 値 | 公開するもの | 受け取ったバッファ |
| --- | --- | --- |
| `off`（既定） | 公開しない | クライアントは `wl_shm` を使う。GL のクライアントは、ソフトウェア描画に切り替える。GPU で描く Vulkan のクライアントは、起動できない |
| `cpu` | `ARGB8888`、`XRGB8888`、`ABGR8888`、`XBGR8888` の、`DRM_FORMAT_MOD_LINEAR` | mmap して、画素を複写する。ドライバーが mmap を許さないバッファ（GPU のメモリの多く）は、表示されない |
| `gpu` | スクリプトの描画のデバイスが取り込める format と modifier | Vulkan に取り込む。複写しない。`--renderer script` が要る |

- 受け付ける format と modifier は、プロトコルの層が決めない。取り込む側が決めて、`ServerConfig::dmabuf` で渡す。
- デバイス（DRM の `major`、`minor`）が分かっていれば、feedback つきの global（version 6）になる。
  分からなければ（lavapipe など）、format の一覧だけの global（version 3）になる。
- **公開した（format、modifier）の組でないバッファは、作成の時点で拒否する。** Smithay が検査するのは format だけなので、
  modifier は、ここで検査する。modifier が示されていないバッファ（`DRM_FORMAT_MOD_INVALID`）は、linear ではない
  （並びは、作ったドライバーだけが知っている）。CPU で読む経路も、明示的な `DRM_FORMAT_MOD_LINEAR` だけを読む。
- **dmabuf を付けた commit は、バッファが読めるようになるまで、適用しない**（Smithay の blocker）。クライアントの GPU が
  描き終わるのを、compositor を止めずに待つ。同じクライアントの、後続の commit も、順番を保って待つ。
  バッファの監視は、`hizukuri-wl` の中のイベントループで行う。
- 取り込めるかどうかは、最初に表示するときまで分からない。取り込めなければ、その表面は何も表示しない。

## ウィンドウ

- 新しい toplevel には、最初の commit で、configure を送る。大きさと activated は、`configure_window` で決める。
  maximized、fullscreen、resizing は、`set_window_states` で決める。
- **クライアントからの要求は、出来事として出すだけで、何もしない。**

| `WindowRequest` | 元の要求 | 補足 |
| --- | --- | --- |
| `Maximize`、`Unmaximize` | `set_maximized`、`unset_maximized` | |
| `Fullscreen { output }`、`Unfullscreen` | `set_fullscreen`、`unset_fullscreen` | クライアントが出力を指定したら、その id |
| `Minimize` | `set_minimized` | |
| `Move` | `move` | その serial のボタンが、まだ押されているときだけ出す |
| `Resize { edges }` | `resize` | 同じ |
| `ShowMenu { position }` | `show_window_menu` | ウィンドウの geometry からの位置 |

- 最大化と全画面の要求には、configure で答える決まりである。ウィンドウ管理が何も送らなければ、次の `flush_clients` で、
  内容の変わらない configure を送る。
- 中核は、要求を `WindowManager::window_request` へ渡す。**Rust のレイアウトも、スクリプトのウィンドウ管理も、
  いまは、要求に応じない**（スクリプトへの口は、未実装である）。

## 出力

- 中核が、ウィンドウのある出力を伝える（`set_window_outputs`）。表面は、その出力にだけ `enter` する。
  望ましい scale（`preferred_buffer_scale`、fractional-scale）は、その出力の scale になる。
- frame callback と presentation の feedback は、ウィンドウの主な出力（一覧の最初）のフレームに合わせて送る。
- まだ置かれていないウィンドウの表面は、どの出力にも入らない。カーソルと drag のアイコンの表面は、
  すべての出力にあるものとして扱う。
- ウィンドウは、1 つの出力にだけ属する。境界をまたいで表示されていても、`enter` するのは片方だけである。

## popup

- 位置は、positioner の求めるとおりに決める。ウィンドウごとの「popup を収める範囲」（`set_popup_bounds`。
  中核は、ウィンドウのある出力を伝える）があれば、positioner が許す調整（flip、slide、resize）で、その中に収める。
- **位置は、クライアントに伝える前に、置き直せる。** popup ができると、`PopupPlacement` を出す。クライアントに
  位置を伝えるのは、次の `flush_clients` のときである。その間に、`place_popup_within`（親の座標での矩形を渡すと、
  positioner の決まりで、その中に収める）か、`set_popup_geometry`（位置を直接決める）を呼べる。
  `popup_request` で、positioner の求めた位置、anchor の矩形、許されている調整が読める。
  ウィンドウを変形して描く側が、画面に出ている範囲に合わせて置くための口である。スクリプトへは、未接続である。
- `xdg_popup.grab` を扱う。grab の間、キーボードは popup に向き、ポインターのイベントは、popup のクライアントにだけ届く。
  そのクライアント以外の場所を押すと、popup を閉じる（`popup_done`）。押した操作は、そのまま続く。
- grab の間に中核が求めた focus の変更は、grab が終わったときに適用する。

## selection と drag and drop

- selection（コピーと貼り付け）: キーボードの focus を持つクライアントだけが、offer を受け取る。
- **selection を設定できるのは、要求を処理する時点で、キーボードの focus を持つクライアントだけである**
  （Smithay の決まり。serial では判定しない）。入力のイベントを受けてから、要求が届くまでの間に focus を失った
  クライアントの要求は、捨てられる。小さなウィンドウを出して focus を得る種類の道具（wl-clipboard）は、
  同時に別のウィンドウが開くと、コピーに失敗することがある。
- いまの selection の MIME type は、`selection_mime_types` で読める（focus に関わらずに調べるため）。
- drag and drop: 相手の表面は、中核の当たり判定の結果で決まる（`pointer_motion` に渡した表面が、offer の相手になる）。
  drag のアイコンの表面は、`drag_icon_surfaces` で取り出せる。スクリプトの描画は、アイコンを描く。
  Rust のレンダラーは、描かない。
- `wl_data_source.target`（相手が受け入れた MIME type を、source に伝えるイベント）は、送られない
  （固定した版の Smithay が、送らない）。

## 入力

- キーボードの focus は、`set_keyboard_focus` で、ウィンドウの root の表面に与える。
- キーは evdev の番号、ポインターのボタンも evdev の番号で受け取る。スクロールは、`wl_pointer.axis` の値と、
  ホイールの高分解能の値（`v120`）で受け取る。
- キーマップは、xkb の既定のもの 1 つである。
