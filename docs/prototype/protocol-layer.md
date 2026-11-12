# プロトコルの層

`prototype/wl` は、Wayland のサーバー側である。Smithay のプロトコルの実装を包み、プロトコルの要求を、
ふつうの状態と出来事に直す。公開する型に、Smithay の型は出てこない。表面、ウィンドウ、出力は id で表し、
画素はバイト列で渡し、入力は数値で受け取る。描画もしないし、ウィンドウをどこに置くかも決めない。

## 呼び出す側との関係

呼び出す側（中核）は、次のように使う。

1. `WlServer::listen_fd` と `WlServer::display_fd` を見張り、readable になったら `accept_clients` と `dispatch_clients` を呼ぶ。
   `display_fd` は、クライアントから要求が届いたときと、待っていたバッファが読めるようになったときに、readable になる。
2. `drain_events` で出来事を取り出して、応じる（ウィンドウを置く、focus を決める、configure を送る）。
3. ウィンドウごとに、表面の一覧（`window_surfaces`）と、commit されたバッファ（`take_buffer_update`）を取り出して描く。
4. フレームの完了を伝え（`send_frame_callbacks`、`send_presented`）、`flush_clients` を呼ぶ。

ポインターとタッチの入力は、表面と、その中の座標で渡す（`pointer_motion`、`touch_down`）。当たり判定は、呼び出す側の仕事である。
ウィンドウを平らに並べても、変形して描いても、同じ口で届く。

出来事（`WlEvent`）:

| 出来事 | 内容 |
| --- | --- |
| `WindowCreated`、`WindowChanged`、`WindowDestroyed` | toplevel ができた、その木のどこかが commit した、無くなった |
| `SurfaceDestroyed` | 表面が無くなった。表面のために持っている資源を捨ててよい |
| `CursorChanged` | ポインターの下のクライアントが求めるカーソルが変わった |
| `WindowRequest { window, request }` | クライアントが、ウィンドウ管理の決めることを求めた（下） |
| `PopupPlacement { window, popup }` | popup ができた、または置き直しを求められた（下） |
| `PointerConstraintsChanged` | ポインターの制約（lock、confine）が求められた、取り下げられた、領域が commit された（下） |
| `PointerLockWithdrawn { surface, hint }` | 有効だった lock を、クライアントが取り下げた。`hint` は cursor position hint（下） |

## 公開しているプロトコル

| global | 版 | 補足 |
| --- | --- | --- |
| `wl_compositor` | 6 | |
| `wl_subcompositor` | 1 | |
| `wl_shm` | 2 | `argb8888`、`xrgb8888`、`abgr8888`、`xbgr8888` |
| `wl_seat` | 9 | キーボードとポインター。タッチは、中核がタッチスクリーンがあると伝えている間だけ（下） |
| `wl_output` | 4 | 出力ごとに 1 つ |
| `wl_data_device_manager` | 3 | selection と drag and drop |
| `xdg_wm_base` | 7 | |
| `zxdg_output_manager_v1` | 3 | |
| `zxdg_decoration_manager_v1` | 1 | どの要求にも、server side と答える |
| `wp_viewporter` | 1 | |
| `wp_presentation` | 2 | |
| `wp_fractional_scale_manager_v1` | 1 | |
| `wp_cursor_shape_manager_v1` | 2 | |
| `zwp_relative_pointer_manager_v1` | 1 | |
| `zwp_pointer_constraints_v1` | 1 | lock と confine。いつ有効にするかは、中核が決める（下） |
| `zwp_pointer_gestures_v1` | 3 | swipe、pinch、hold。ポインターのある表面に届く（下） |
| `zwp_primary_selection_device_manager_v1` | 1 | |
| `ext_data_control_manager_v1` | 1 | primary selection も扱う。どのクライアントにも見せる（下） |
| `zwlr_data_control_manager_v1` | 2 | 同じ |
| `zwp_text_input_manager_v3` | 1 | アプリの入力欄（[入力メソッド](#入力メソッド)） |
| `zwp_input_method_manager_v2` | 1 | 入力メソッド（IME）。どのクライアントにも見せる |
| `zwp_virtual_keyboard_manager_v1` | 1 | 仮想キーボード。どのクライアントにも見せる |
| `xdg_activation_v1` | 1 | activate の要求を、ウィンドウ管理に渡す（下） |
| `zwp_linux_dmabuf_v1` | 6 または 3 | `--dmabuf` で公開したときだけ（下） |
| `wp_color_manager_v1` | 3 | parametric な image description だけ。ICC は公開しない（下） |

未実装: `wp_color_representation_v1`、`zwp_tablet_manager_v2`。

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
  dmabuf は mmap して複写する。`--dmabuf cpu` で受け取ったバッファを、`wl_shm` と同じように複写して使うための、遅い経路である。

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
| `cpu` | `ARGB8888`、`XRGB8888`、`ABGR8888`、`XBGR8888` の、`DRM_FORMAT_MOD_LINEAR` | `wl_shm` と同じように、mmap して、画素を複写する。プロトコルを試すための経路である。ドライバーが mmap を許さないバッファ（GPU のメモリの多く）は、表示されない |
| `gpu` | スクリプトの描画のデバイスが取り込める format と modifier | Vulkan に取り込む。複写しない。`--no-drawing` とは一緒に使えない |

- 受け付ける format と modifier は、プロトコルの層が決めない。取り込む側が決めて、`ServerConfig::dmabuf` で渡す。
- デバイス（DRM の `major`、`minor`）が分かっていれば、feedback つきの global（version 6）になる。
  分からなければ（lavapipe など）、format の一覧だけの global（version 3）になる。
- **公開した（format、modifier）の組でないバッファは、作成の時点で拒否する。** Smithay が検査するのは format だけなので、
  modifier は、ここで検査する。modifier が示されていないバッファ（`DRM_FORMAT_MOD_INVALID`）は、linear ではない
  （並びは、作ったドライバーだけが知っている）。CPU で読む経路も、明示的な `DRM_FORMAT_MOD_LINEAR` だけを読む。
- **dmabuf を付けた commit は、バッファが読めるようになるまで、適用しない**（Smithay の blocker）。クライアントの GPU が
  描き終わるのを、compositor を止めずに待つ。同じクライアントの、後続の commit も、順番を保って待つ。
  バッファの監視は、`proto-wl` の中のイベントループで行う。
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
| `Move` | `move` | その serial のボタンが、まだ押されているとき、またはその serial のタッチ（画面に触れている指のうち、最初の指）が、まだ離れていないときだけ出す |
| `Resize { edges }` | `resize` | 同じ |
| `ShowMenu { position }` | `show_window_menu` | ウィンドウの geometry からの位置 |
| `Activate { from_input }` | `xdg_activation_v1.activate` | token が、キーボードの focus を持つクライアントの、focus が移った後の入力の serial で作られたなら、`from_input` が真 |

- 最大化と全画面の要求には、configure で答える決まりである。ウィンドウ管理が何も送らなければ、次の `flush_clients` で、
  内容の変わらない configure を送る。
- 中核は、要求を `WindowManager::window_request` へ渡す。fallback のウィンドウ管理は、要求に応じない。
  スクリプトのウィンドウ管理は、スクリプトの `window_request` に渡す（[スクリプトの層](script-layer.md)）。
- xdg-activation の token は、すべて発行する。1 つの token は、1 回の activate に使える。activate の要求に従うか
  （focus を奪わせるか）は、ウィンドウ管理が決める。

`window` が返す `WindowInfo` は、題名、app id、大きさ、状態のほかに、次のものを持つ。

| 項目 | 内容 |
| --- | --- |
| `parent` | `xdg_toplevel.set_parent` の相手のウィンドウ。ダイアログの、もとのウィンドウ |
| `min_size`、`max_size` | `set_min_size`、`set_max_size` の値（論理座標）。0 は制限なし |
| `client_pid` | クライアントのプロセスの id（接続の相手を、kernel が答えたもの）。xwayland-satellite のウィンドウを見分けるのに使う |

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
  タッチは grab されない。popup のクライアントの表面へのタッチは、そのまま届く。ほかのクライアントの表面か、
  表面の無い所に触れると、popup を閉じ、そのタッチは、触れた表面に届く。
- grab の間に中核が求めた focus の変更は、grab が終わったときに適用する。
- 入力メソッドがキーボードを grab しているときに開いた popup の grab は、キーボードの grab を取らない（下の
  [入力メソッド](#入力メソッド)）。

## selection と drag and drop

- selection（コピーと貼り付け）: キーボードの focus を持つクライアントだけが、offer を受け取る。
- **selection を設定できるのは、要求を処理する時点で、キーボードの focus を持つクライアントだけである**
  （Smithay の決まり。serial では判定しない）。入力のイベントを受けてから、要求が届くまでの間に focus を失った
  クライアントの要求は、捨てられる。小さなウィンドウを出して focus を得る種類の道具は、
  同時に別のウィンドウが開くと、コピーに失敗することがある。wl-clipboard は、data control（下）があれば、
  ウィンドウを出さずに、それを使う。
- いまの selection の MIME type は、`selection_mime_types` で読める（focus に関わらずに調べるため）。
- drag and drop: 相手の表面は、中核の当たり判定の結果で決まる（`pointer_motion` に渡した表面が、offer の相手になる）。
  タッチで始める drag and drop（タッチの serial での `start_drag`）は、受け付けない（source を取り消す）。
  drag のアイコンの表面は、`drag_icon_surfaces` で取り出せる。スクリプトの描画は、アイコンを描く。
- `wl_data_source.target`（相手が受け入れた MIME type を、source に伝えるイベント）は、送られない
  （固定した版の Smithay が、送らない）。

### primary selection と data control

- primary selection（選んだ文字列を、中ボタンで貼り付ける）は、selection と同じ決まりで動く。
  キーボードの focus を持つクライアントだけが、offer を受け取り、設定できる。
  いまの MIME type は、`primary_selection_mime_types` で読める。
- data control（`ext_data_control_manager_v1`、`zwlr_data_control_manager_v1`）は、focus と関係なく、
  selection と primary selection を読み、置き換える口である。クリップボードの履歴の道具が使う
  （wl-clipboard は、これがあると、自分のウィンドウを開かずに読み書きする）。
- **data control は、どのクライアントにも見せている。** どのクライアントも、focus なしに、クリップボードを読み、
  置き換えられる。sandbox の中のクライアントを見分ける仕組み（`wp_security_context_v1`）は、未実装である。
- selection の持ち主が source を消したとき、または切断したとき、Smithay は selection を消すが、
  `SelectionHandler::new_selection` を呼ばない。`selection_mime_types` と `primary_selection_mime_types` は、
  そのため、覚えておいた値を返す前に、selection がまだあるかを Smithay に確かめる
  （どの source も持てない MIME type で読み出しを求めると、送らずに、「selection が無い」か「その型は無い」で失敗する）。

## 相対的な動きと、ポインターの制約

- 相対的な動き（`zwp_relative_pointer_v1.relative_motion`）は、`pointer_relative_motion` で送る。
  加速を掛けた後の値と、掛ける前の値を、両方受け取る。送り先は、ポインターのある表面のクライアントである。
  出力の端で、ポインターが止まっても、動きは、そのまま送る。
- ポインターの制約（`zwp_pointer_constraints_v1` の lock と confine）は、Smithay が、表面ごとに、プロトコルの状態を持つ
  （表面に 1 つだけ、double-buffered の領域と cursor position hint、`locked` を 1 度だけ送ること、
  oneshot は 1 度解けたら消えること）。この層は、それを id で見せる。

| 口 | 内容 |
| --- | --- |
| `pointer_constraints`、`pointer_constraint` | 求められている制約。表面、ウィンドウ、種類（lock、confine）、有効かどうか、cursor position hint |
| `pointer_constraint_region_contains` | 表面の中の点が、制約の領域（指定が無ければ表面の全体）と input region の、両方の中にあるか |
| `set_pointer_constraint_active` | 有効にする、解く。有効にできるのは、ポインターが、その表面にあるときだけである（プロトコルが、クライアントに約束していること） |

- **いつ有効にし、いつ解くかは、この層は決めない。** 例外は 1 つで、ポインターが表面を離れると、Smithay が解く。
- 中核が、それを決める（[スクリプトの層](script-layer.md#ポインターの制約)）。

## 入力メソッド

text-input-v3（アプリ）、input-method-v2（IME）、virtual-keyboard-v1（IME が返すキー）を扱う。
2 つの側の状態の受け渡し（enable と activate、preedit と commit、`done` の serial）は、Smithay が行う。
プロトコルの要約は、[IME のプロトコル](../research/wayland/ime.md)にある。

- text-input の focus は、キーボードの focus に従う。キーボードの focus が動くと、IME は deactivate される。
  入力メソッドのクライアントがいないときは、アプリの text-input に `enter` を送らない。
- 入力メソッドは、seat に 1 つである。2 つ目の `zwp_input_method_v2` には、`unavailable` を送る。
- **キーボードの grab。** IME が grab している間、`keyboard_key` で渡したキーは、IME に届き、アプリには届かない。
  中核のキーの割り当て（緊急用のキー、スクリプトの `key`）は、`keyboard_key` を呼ぶ前に、中核が見る。
  grab より先である。
- **仮想キーボードのキー。** grab を持つ IME の仮想キーボードのキーは、grab を通さずに、キーボードの focus に届く
  （grab へ戻すと、IME との間で回り続ける）。キーの前に、いまの modifier を送る（grab の間、modifier の変化は、
  grab にだけ届いているため）。IME が仮想キーボードの `modifiers` で返す modifier も、focus に送る。
  押したキーの「離す」は、IME がいなくなった後に届いても、同じ経路で送る。grab の前にアプリに届いたキーの「離す」を
  grab が受け取り、IME が返したときも、アプリに送る（ショートカットの modifier を押したまま focus が移ったとき）。
  IME が押しを使ったキーの「離す」を返してきても（fcitx5 は、そうする）、アプリには送らない。
  ほかのクライアントの仮想キーボードのキーは、実際のキーボードのキーと同じ経路を通る（grab にも届く）。
  その keymap が seat のものと違えば、その間だけ seat の keymap を置き換え、実際のキーボードのキーが来たら戻す。
  中核のキーの割り当ては、仮想キーボードのキーを見ない。
- **IME が落ちたとき。** grab は外れ、アプリの text-input は `leave` を受け取り、キーは、またアプリに直接届く。
  IME の仮想キーボードで押されたままのキーは、離される。
- **候補ウィンドウ（`zwp_input_popup_surface_v2`）。** IME が active で、text-input に focus があるときだけ、見せる。
  focus のある text-input の表面が属するウィンドウの、表面の 1 つ（`SurfaceRole::InputPopup`）として、
  `window_surfaces` のいちばん手前に入る。中核とスクリプトは、ほかの表面と同じように描き、当たり判定をする。
  - 位置は、プロトコルの層が決める。アプリが `set_cursor_rectangle` で伝えた矩形の下に、左端をそろえて置く。
    ウィンドウの「popup を収める範囲」（`set_popup_bounds`）があり、下に収まらず、上なら収まるときは、矩形の上に置く。
    左右は、範囲に収まるようにずらす。
  - IME には、カーソルの矩形を、候補ウィンドウの表面から見た座標で伝える（`text_input_rectangle`）。
- `input_method_status` で、入力メソッドの有無、active か、grab しているか、候補ウィンドウの位置と、
  アプリが伝えたカーソルの矩形が読める。
- 入力メソッドがキーボードを grab している間に、アプリが `xdg_popup.grab` で popup（メニュー）を開くと、
  キーボードの grab は IME に残し、キーボードの focus を popup に移す。focus は、popup が閉じるまで popup に留まる。
  キーは IME を通り、IME が使わなかったキーが、仮想キーボードで popup に届く。
- text-input-v3 の version 2（`set_cursor_rectangle` を `wl_surface.commit` に合わせる）は、扱わない
  （固定した版の Smithay が、version 1 だけを公開する）。

## 色

`wp_color_manager_v1`（color-management-v1）は、Smithay に実装が無いので、この層が自分で受け答えする（`prototype/wl/src/color.rs`）。
プロトコルの記述は、すべて `proto_color::ImageDescription` に直す。変換はしない。表面の image description は、
`SurfaceNode` の `image_description` として、描く側に渡る。

公開するもの:

| 種類 | 内容 |
| --- | --- |
| rendering intent | `perceptual`、`relative`。どちらも、白を白へ写し（色順応）、出力の外を切り捨てる |
| feature | `parametric`、`set_primaries`、`set_tf_power`、`set_luminances`、`set_mastering_display_primaries`、`extended_target_volume`、`windows_scrgb`。version 3 で bind したクライアントには、`windows_bt2100` も |
| transfer function | `bt1886`、`gamma22`、`gamma28`、`ext_linear`、`st2084_pq`、`hlg`。version 2 以上では `compound_power_2_4`（IEC 61966-2-1 の区分的な関数）、version 1 では `srgb`（指数 2.2 のべき乗として扱う） |
| primaries | 10 種のすべて（`srgb`、`pal_m`、`pal`、`ntsc`、`generic_film`、`bt2020`、`cie1931_xyz`、`dci_p3`、`display_p3`、`adobe_rgb`） |

- **ICC は、公開しない。** `icc_v2_v4` を公開しないので、`create_icc_creator` は、プロトコルの決まりのとおり、
  `unsupported_feature` のエラーになる。
- parametric な記述は、`create` の時点で、`ready`（version 1）か `ready2`（version 2 以上）を返す。決まりに反してはいないが、
  変換できない記述（原色が縮退している、など）は、`failed`（`unsupported`）で返す。
  決まりに反するもの（同じ項目の 2 度目の設定、足りない項目、公開していない名前、輝度の大小）は、仕様のエラーにする。
- PQ では、`set_luminances` の最大は使わない（最小 + 10000 cd/m² とする）。
- `create_windows_scrgb` は、BT.709 の原色、linear、0〜80 cd/m²（値 1.0 が 80 cd/m²）、reference white 203 cd/m² である。
  `create_windows_bt2100` は、BT.2020 の原色、PQ、最小 0、reference white 203 cd/m² である（プロトコルが勧める仮定）。
- mastering display の原色と輝度、max_cll、max_fall は、`ImageDescription::mastering` に入れる。変換には使わない。
- image description の identity は、1 から数え、使い回さない。出力の記述は、変わるまで同じ identity を持つ。

表面:

- 設定（`wp_color_management_surface_v1.set_image_description`、`unset_image_description`、object の破棄）は、
  Smithay の表面の cached state に置く。`wl_surface.commit` で有効になり、同期した subsurface では、親の commit で有効になる。
  設定は写しなので、直後に image description を破棄しても、変わらない。
- 1 つの表面に、object は 1 つだけである（2 つ目は `surface_exists`）。表面が消えた object への要求は、`inert` のエラーになる。
- 設定の無い表面は、`DEFAULT_SURFACE_IMAGE_DESCRIPTION`（BT.709、指数 2.2 のべき乗）として扱う。
- `WlServer::set_surface_color`（制御口の `set_surface_color`）は、プロトコルの設定より優先する。alpha mode も、これで決める
  （color-representation-v1 は未実装。プロトコルで設定した表面は、`premultiplied_electrical` である）。
  `SurfaceNode::color_source` が、どれが効いているか（`Control`、`Protocol(intent)`、`Default`）を示す。

出力と feedback:

- 出力の image description は、`OutputConfig::image_description` である。`update_output` で変わると、その出力の
  `wp_color_management_output_v1` に `image_description_changed` を送り、続けて `wl_output.done` を送る。
- 出力の記述の中身（`get_information`）は、bind された版で送れる形で送る。区分的な sRGB の関数は、version 1 には名前が無いので、
  version 1 のクライアントには `failed`（`low_version`）を返す。
- 表面に望む image description（`wp_color_management_surface_feedback_v1`）は、表面の主な出力（ウィンドウの出力の一覧の最初）の
  記述である。出力の無い表面は、最初の出力の記述、出力が 1 つも無ければ、設定の無い表面と同じ記述である。
  feedback を作ったときと、望む記述が変わったとき（ウィンドウの出力、出力の記述、出力の増減）に、`preferred_changed`
  （version 2 以上では `preferred_changed2`）を送る。
- `wp_image_description_info_v1` の中身は、要求を処理した dispatch の後で送る（`done` が object を消すので、
  作った要求の処理の中では送れない）。

## タッチ

| 口 | 内容 |
| --- | --- |
| `set_touch_available` | seat の capability に touch を出す、取り下げる。中核は、タッチスクリーンがある間だけ出す。取り下げると、触れている指は忘れる |
| `touch_down` | 指が、表面の中の座標に触れた（表面が無い所なら `None`）。`slot` は、離れるまで、その指の名前である |
| `touch_motion` | 指が動いた。座標は、触れた表面の座標である |
| `touch_up`、`touch_frame`、`touch_cancel` | 指が離れた。同時に起きたタッチの区切り（`wl_touch.frame`）。進行中のすべてのタッチを取り消す（`wl_touch.cancel`） |
| `touch_points` | いま触れている指。slot と、触れた表面 |

- **タッチは、触れた表面のものである。** 指が離れるまで、その指の動きは、その表面に届く。指がほかのウィンドウの上や、
  ウィンドウの無い所へ動いても、変わらない。どの表面か、表面の中のどこかは、呼び出す側の当たり判定が決める。
- **指ごとに、表面が決まる。** 2 本の指が、2 つのウィンドウに触れれば、それぞれのウィンドウに届く。
  `frame` は、その区切りのイベントを受け取ったクライアントのそれぞれに、1 度ずつ送る。
- `touch_cancel` は、それまでの区切りのイベントを受けた指も含めて、すべての指を取り消す。取り消した指の up は、送らない。
  取り消す前に、各指に、いまの位置への motion を 1 つ送る（固定した版の Smithay が、組み立て中の区切りに入っている指しか
  取り消さないため）。
- 触れた表面が消えても、その指の up は、クライアントに届く。motion は、呼び出す側が座標を出せないので、届かない。
- タッチでの move と resize（`xdg_toplevel.move` をタッチの serial で求める）は、ボタンと同じく、ウィンドウ管理への要求として出す。
- pointer の emulation（`wl_touch` を持たないクライアントに、タッチをポインターの操作として送ること）は、しない。
  タッチの shape と orientation は、送らない。

## タッチパッドのジェスチャー

- `gesture_begin`、`gesture_update`、`gesture_end` で、swipe、pinch、hold を送る。送り先は、ポインターのある表面の
  クライアントである。
- swipe の `update` は、指の中心の動き。pinch の `update` は、それに加えて、始めからの scale と、前の update からの回転（度）。
  hold には、update が無い。
- 進行中のジェスチャーの表面から、ポインターが離れると（ほかのウィンドウが下に来た、ウィンドウが動いた）、
  そのクライアントには `end(cancelled)` が届き、残りは、どのクライアントにも届かない（Smithay の振る舞い）。
- 誰に送るか（コンポジターが自分で使うか）は、この層は決めない。中核が、送るものだけを渡す
  （[スクリプトの層](script-layer.md#タッチとジェスチャー)）。

## 入力

- キーボードの focus は、`set_keyboard_focus` で、ウィンドウの root の表面に与える。
- キーは evdev の番号、ポインターのボタンも evdev の番号で受け取る。スクロールは、`wl_pointer.axis` の値と、
  ホイールの高分解能の値（`v120`）で受け取る。
- キーマップは、xkb の既定のもの 1 つである。
