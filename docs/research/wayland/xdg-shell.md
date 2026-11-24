# xdg-shell と、xdg-decoration、fractional-scale、viewporter、presentation-time

- 確認した版: wayland-protocols 1.50（`3eab2dccf4bf`）、wayland 1.26.0、Smithay（`8fd9fe0c74e5`）

| プロトコル | interface の版 | Smithay（`8fd9fe0c`）が公開する版 |
| --- | --- | --- |
| xdg-shell（stable） | `xdg_wm_base` version 7 | 7 |
| xdg-decoration-unstable-v1 | `zxdg_decoration_manager_v1` version 2 | 1 |
| fractional-scale-v1（staging） | `wp_fractional_scale_manager_v1` version 1 | 1 |
| viewporter（stable） | `wp_viewporter` version 1 | 1 |
| presentation-time（stable） | `wp_presentation` version 3 | 2 |

xdg-shell の状態管理は、Smithay が持っている。compositor の側で決めるのは、
「どういう大きさと状態を configure で伝えるか」と、「popup をどこに置くか」である。
この文書は、それを決めるために必要な範囲をまとめる。

## 1. xdg-shell

### 目的

ふつうのアプリのウィンドウ（toplevel）と、メニューなどの一時的な表面（popup）を扱う。
要点は、**大きさと状態を、compositor とクライアントが交渉して決める**ことである。

### 登場する object

| interface | 役割 |
| --- | --- |
| `xdg_wm_base` | global。`xdg_surface` と `xdg_positioner` を作る。ping と pong で、クライアントの応答を確かめる |
| `xdg_surface` | `wl_surface` に、ウィンドウとしての土台を与える。これだけでは role にならない |
| `xdg_toplevel` | ウィンドウの role |
| `xdg_popup` | popup の role |
| `xdg_positioner` | popup の置き方の規則を集める object |

### configure と ack の流れ

**最初に表示するまで。**

1. クライアントが `wl_surface` を作り、`get_xdg_surface`、`get_toplevel` を呼ぶ。
2. クライアントが、タイトルや app_id などを設定する。
3. クライアントが、**バッファを付けずに** `wl_surface.commit` する（initial commit）。
4. compositor が、`xdg_toplevel.configure`（幅、高さ、状態の配列）を送り、続けて `xdg_surface.configure`（serial）を送る。
5. クライアントが、`xdg_surface.ack_configure`（serial）を送る。
6. クライアントが、バッファを付けて commit する。ここで、表面が map される。

- configure より前にバッファを付けると、`unconfigured_buffer` のエラーになる。
- バッファがすでに付いている `wl_surface` から `xdg_surface` を作るのも、エラーである。

**configure sequence。**

- role ごとのイベント（`xdg_toplevel.configure`、`xdg_popup.configure` など）は、
  最後の `xdg_surface.configure` でひとまとまりになる。まとめて 1 つの状態として扱う。
- xdg-decoration の `configure` も、このまとまりの中に入る。

**ack の決まり。**

- クライアントは、configure に応じた内容を commit する前に、その serial を ack する。
- 複数の configure が届いたら、最後のものだけ ack すればよい。
- ack した serial は、「commit した内容が、どの configure に応じたものか」を表す。
  commit の前に何度 ack してもよいが、意味を持つのは、commit の直前の ack である。
- 送っていない serial の ack と、すでに過ぎた serial の ack は、`invalid_serial` のエラーにする。
- **compositor は、ack を伴う commit が来るまで、新しい状態が反映されたと見なしてはならない。**
  たとえば、最大化の configure を送っても、クライアントがその大きさで描いて commit するまでは、古い内容が届く。

**toplevel の configure の意味。**

- 幅と高さは、window geometry（後述）の大きさである。
- 幅か高さが 0 のときは、「クライアントが決めてよい」という意味である。
- 状態の配列は、今の状態の全部を毎回送る。

| 状態 | 入った版 | 意味 |
| --- | --- | --- |
| `maximized` | 1 | 最大化している。大きさは、指定のとおりにしなければならない |
| `fullscreen` | 1 | 全画面。大きさは上限であり、それより小さくてもよい |
| `resizing` | 1 | 大きさを変えている途中 |
| `activated` | 1 | 見た目の上で active である。focus と同じとは限らない |
| `tiled_left`、`tiled_right`、`tiled_top`、`tiled_bottom` | 2 | その辺が、ほかのものに接している。その辺の影や角丸を描かないための合図 |
| `suspended` | 6 | 見えていないので、描画を止めてよい |
| `constrained_left` など 4 つ | 7 | その辺の方向には、大きさを変えられない |

そのほかの toplevel のイベント:

| イベント | 入った版 | 意味 |
| --- | --- | --- |
| `close` | 1 | 閉じる要求。クライアントは、従わなくてもよい |
| `configure_bounds` | 4 | 最大化していないときに使える領域の目安 |
| `wm_capabilities` | 5 | compositor が対応している操作（ウィンドウメニュー、最大化、全画面、最小化） |

### window geometry

- `xdg_surface.set_window_geometry` は、ユーザーから見た「ウィンドウの範囲」を、表面の座標で伝える。
- client-side decoration の影のような、見えるけれどウィンドウの範囲ではない部分を除くためのものである。
- configure の大きさ、popup の位置、ウィンドウの整列は、すべて window geometry を基準にする。
- 1 度も設定されていなければ、subsurface を含めた表面全体が、window geometry になる。
- double-buffered である。大きさが 0 以下だと、`invalid_size` のエラーになる。

### popup の位置決め

popup は、親の表面（toplevel か、別の popup）に対する相対的な位置に置く。
クライアントは位置を直接は指定せず、`xdg_positioner` に規則を入れて渡す。
compositor は、その規則に従って位置を計算する。

**`xdg_positioner` の項目。**

| 要求 | 内容 |
| --- | --- |
| `set_size` | popup の大きさ（window geometry）。必須 |
| `set_anchor_rect` | 親の window geometry の中の、基準になる矩形。必須 |
| `set_anchor` | anchor rect のどの辺、どの角を基準にするか |
| `set_gravity` | 基準の点から、popup をどちらの向きに置くか |
| `set_constraint_adjustment` | はみ出すときの調整の方法（下の表） |
| `set_offset` | 追加のずらし |
| `set_reactive`（version 3） | 親が動いたときなどに、compositor が位置を計算し直してよい |
| `set_parent_size`（version 3） | 親の大きさの予定。親の大きさが変わる途中で使う |
| `set_parent_configure`（version 3） | この positioner が、親のどの configure に応じたものか |

- 大きさと anchor rect が設定されていない positioner を使うと、`invalid_positioner` のエラーになる。

**はみ出すときの調整。** bit の組み合わせで指定する。

| 調整 | 内容 |
| --- | --- |
| `slide_x`、`slide_y` | 収まるように、位置をずらす |
| `flip_x`、`flip_y` | anchor と gravity を反対にして、反対側に置く |
| `resize_x`、`resize_y` | 収まるように、大きさを縮める |

- 「どの範囲に収めるか」は、compositor が決める。ふつうは、出力の使える領域である。
- 1 つの軸に、複数の調整が指定されているときは、flip、slide、resize の順に試す。
- flip した結果も収まらないときは、flip する前の位置に戻す。
- slide は、まず gravity の向きへ、次に反対の向きへ、ずらす。
- どの bit も立っていない軸では、はみ出しても、位置を変えない。

**popup の流れ。**

1. クライアントが positioner を作り、`get_popup`（親、positioner）を呼ぶ。
2. 必要なら、`xdg_popup.grab`（seat、serial）を呼ぶ。map した後では呼べない（`invalid_grab`）。
3. initial commit をする。
4. compositor が、位置を計算し、`xdg_popup.configure`（x、y、幅、高さ）と `xdg_surface.configure` を送る。
   位置は、親の window geometry の左上からの相対位置である。
5. クライアントが ack して、バッファを付けて commit する。

- 親は、popup より先に map されていなければならない。
- version 2 までは、popup の configure は最初の 1 度だけである。
  version 3 からは、reactive な positioner のときと、`reposition` の応答として、もう 1 度送れる。
- `reposition`（version 3）: map した後の popup を、新しい positioner で置き直す。
  compositor は、`repositioned`（token）、`xdg_popup.configure`、`xdg_surface.configure` を続けて送る。
  新しい位置は、クライアントが ack するまで有効にならない。

**grab と、閉じる順序。**

- grab した popup は、ユーザーが外側をクリックしたときなどに、compositor が閉じる。
  閉じるときは、`popup_done` を送る。
- grab している間、そのクライアントの表面には、ポインターとタッチのイベントがふつうに届く。
  ほかのクライアントの表面には届かない。キーボードの focus は、最も上の grab している popup に移る。
- grab する popup の親は、toplevel か、grab している別の popup でなければならない。
- 入れ子の popup は、作った順の逆に消さなければならない。
  最も上でない popup を消すと、`not_the_topmost_popup` のエラーになる。
- compositor が閉じるときも、同じ順で閉じる。
- grab の serial は、ユーザーの操作（ボタン、キー、タッチ）のイベントのものでなければならない。
  compositor は、正しくない serial の grab を断り、popup をすぐに閉じる。

### compositor が守るべき決まり

1. initial commit に応えて、必ず configure を送る。
2. configure の serial を管理する。ack の検査をする（`invalid_serial`）。
3. ack を伴う commit を見てから、新しい状態が反映されたと扱う。
4. configure sequence の中の role のイベントを、まとまりとして扱う。
5. 大きさは、window geometry を基準にする。
6. popup の位置は、positioner の規則で計算する。親の window geometry からの相対位置で伝える。
7. popup を閉じる順序を守る。閉じるときは `popup_done` を送る。
8. role の付け替えを許さない（`role` のエラー）。`xdg_surface` を、role の object より先に消させない（`defunct_role_object`）。
9. `xdg_wm_base` を、子の object より先に消させない（`defunct_surfaces`）。
10. ping に応えないクライアントを、応答なしとして扱える（閉じるかどうかは、compositor の方針である）。

### 落とし穴

- **configure を送りすぎる。** 大きさを変えている途中に、動きのたびに configure を送ると、クライアントが追いつかない。
  ack を待ってから次を送る、という調整が要る。
- **クライアントは、指定した大きさに従うとは限らない。** 最大化していないときの大きさは、希望にすぎない。
  端末は、文字の大きさの倍数に丸める。最小と最大の大きさ（`set_min_size`、`set_max_size`）もある。
  タイル型のレイアウトでは、指定と違う大きさのバッファが届いたときの扱い（余白、切り取り、中央寄せ）を決めておく。
- **レイアウトの変更と、内容の更新の時機を合わせる。** 複数のウィンドウの大きさを同時に変えるとき、
  それぞれのクライアントが新しい大きさで描くまでの時間が違う。そのまま表示すると、一瞬、隙間や重なりが見える。
  そろえるには、すべてのクライアントの、ack を伴う commit を待ってから、配置を切り替える。
- **`activated` と、キーボードの focus は別である。** `activated` は見た目のための状態である。
- **`suspended` を使う。** 見えていないウィンドウ（別のワークスペース、画面の外へスクロールしたもの）に送ると、
  クライアントが描画を止められる。frame callback を送らないことと組み合わせる。
- **popup を収める範囲。** 収める範囲は、親の表面の座標で考える。ウィンドウを変形して表示する場合、
  「出力の使える領域」を、親の表面の座標の矩形に直せないことがある。

## 2. xdg-decoration

### 目的

ウィンドウの装飾（タイトルバー、枠）を、クライアントが描くか、compositor が描くかを交渉する。

- このプロトコルで交渉しなければ、クライアントは自分で装飾を描く。
- `zxdg_toplevel_decoration_v1` は、toplevel につき 1 つである（`already_constructed`）。
  toplevel より先に消さなければならない（`orphaned`）。

### 流れ

1. クライアントが、`get_toplevel_decoration` で object を作る。
2. クライアントが、`set_mode`（`client_side` か `server_side`）で希望を伝える。`unset_mode` は、希望なしである。
3. compositor が、`configure`（mode）を送り、`xdg_surface.configure` で締める。
4. クライアントは、**指定された mode に従わなければならない**。ack して、その mode で描く。

- compositor は、クライアントの希望と違う mode を指定してよい。
- compositor は、いつでも configure を送って、mode を変えられる。
- object を消すと、次の commit で、client-side に戻る。
- 範囲の外の mode は、`invalid_mode` のエラーにする。

### 版の違い

- version 1: バッファがすでに付いている toplevel から object を作るのは、エラーである（`unconfigured_buffer`）。
- version 2: 作ってよい。
- Smithay が公開するのは version 1 である。

### 落とし穴

- **対応していないクライアントがある。** このプロトコルを使わないクライアントは、自分で装飾を描く。
- `server_side` を指定しても、compositor が装飾を描く義務はない。「クライアントは描かない」という意味になる。
  枠もタイトルバーも無いウィンドウにできる。
- KDE の古いプロトコル（`org_kde_kwin_server_decoration`）しか使わないクライアントがある。
  Smithay は、これも持っている。

## 3. fractional-scale と viewporter

### 目的

- **viewporter**: 表面の内容を、切り取りと拡大縮小で、バッファと違う大きさで表示する。
- **fractional-scale**: 整数でない倍率（1.5 倍など）を、compositor が表面に勧める。

fractional-scale は、viewporter と組み合わせて使う。

### viewporter

- `wp_viewporter.get_viewport` で、表面ごとの `wp_viewport` を作る。表面につき 1 つである。
- `set_source`（x、y、幅、高さ。固定小数点）: バッファのどの部分を使うか。
- `set_destination`（幅、高さ。整数）: 表面の大きさ。
- どちらも double-buffered である。
- destination を設定すると、**表面の大きさが、バッファの大きさではなく、destination になる**。
- source だけを設定した場合、source の幅と高さは整数でなければならない（`bad_size`）。表面の大きさは、source の大きさになる。
- 座標の変換の順序は、buffer transform、buffer scale、viewport の順である。
  source の矩形は、transform と scale を適用した後の座標で指定する。
- source が負の位置を持つと `bad_value`、バッファの外にはみ出すと `out_of_buffer` のエラーになる。
  これらの検査は、状態が適用されるとき（commit）に行う。
- 表面が消えた後の要求は、`no_surface` のエラーになる。

### fractional-scale

- `wp_fractional_scale_manager_v1.get_fractional_scale` で、表面ごとの object を作る。
- compositor が、`preferred_scale` を送る。値は、**分母が 120 の分数の分子**である。1.5 倍なら 180 である。
- クライアントは、次のようにする。
  - バッファを、表面の大きさに倍率を掛けた大きさで描く。
  - `wl_surface` の buffer scale は 1 のままにする。
  - `wp_viewport` の destination に、表面の大きさを設定する。
- 例（XML の説明）: 表面が 100x50 で、倍率が 1.5 なら、バッファは 150x75、destination は 100x50 である。
- toplevel の大きさの丸めは、「0 から遠いほうへの四捨五入」と決まっている。
  subsurface の位置と大きさの丸めは、決まっていない。

### `wl_surface` の preferred_buffer_scale との関係

- `wl_compositor` の version 6 から、`wl_surface.preferred_buffer_scale` と `preferred_buffer_transform` がある。
  compositor が、整数の倍率と、向きを表面に勧める。
- fractional-scale に対応していないクライアントは、こちらを使う。
- compositor は、両方を送る。整数のほうの値の決め方は、プロトコルでは決まっていない（0 より大きい値であること、だけが決まっている）。
  小数の倍率の出力では、切り上げた値を送ると、クライアントは、足りる解像度で描く。

### 落とし穴

- **論理的な座標は整数である。** Wayland の表面の大きさと位置は、整数の論理的な画素である。
  倍率が整数でないと、論理的な画素の境界が、物理的な画素の境界に合わない。
  配置を小数で計算し、物理的な画素に合わせて丸める必要がある。
- **丸めの不一致。** compositor とクライアントで、バッファの大きさの丸めが違うと、1 画素ずれて、ぼやける。
- xwayland-satellite は、拡大縮小のために viewporter を必須にしている。

## 4. presentation-time

### 目的

クライアントが commit した内容が、**実際にいつ画面に出たか**を、クライアントに知らせる。
動画の再生やゲームが、音と映像を合わせたり、フレームの間隔を整えたりするのに使う。

### 流れ

1. bind すると、compositor が `clock_id` を送る。時刻の基準になる時計（`clock_gettime` の ID）である。
2. クライアントが、commit の前に `wp_presentation.feedback`（表面）を呼ぶ。その commit の内容についての feedback を求める。
3. その内容が画面に出たら、compositor が、まず `sync_output`（どの出力の更新に合わせたか）を送り、
   続けて `presented` を送る。
4. その内容が 1 度も画面に出なかったら（次の commit で上書きされた、など）、`discarded` を送る。

`presented` と `discarded` は、どちらも object を消す。必ず、どちらか 1 つを送る。

### `presented` の中身

| 引数 | 内容 |
| --- | --- |
| 時刻 | その内容が、最初に光になった時刻。`clock_id` の時計で表す |
| `refresh` | 次の更新までの時間の予測（ナノ秒）。予測できなければ 0 |
| `seq` | 出力の更新の通し番号（vblank の回数に当たる） |
| `flags` | 下の表 |

| flag | 意味 |
| --- | --- |
| `vsync` | 表示装置の垂直同期に合わせた。ティアリングは起きていない |
| `hw_clock` | 時刻は、ハードウェアが測ったものである |
| `hw_completion` | 表示の切り替えの完了を、ハードウェアが知らせた |
| `zero_copy` | クライアントのバッファを、コピーせずに表示装置へ渡した（direct scanout）。GPU での合成は、コピーに数える |
| `fixed_rate`（version 3） | 表示装置が、固定のリフレッシュレートで動いている。`refresh` は 0 であってはならない |

### 版の違い

- version 1: 出力のリフレッシュレートが一定でないとき、`refresh` は 0 でなければならない。
- version 2: 一定でないとき、compositor が選んだ値（最も速いレートなど）か、0 にする。
- version 3: `fixed_rate` の flag が加わった。
- Smithay が公開するのは version 2 である。

### 落とし穴

- **時刻は、KMS の page flip の完了のイベントから取る。** commit した時刻や、描画した時刻ではない。
- **ティアリングで出したフレームには、`vsync` を付けない。**
- nested と headless では、ハードウェアの時刻が無い。nested では、host の presentation-time の値を使える。
  headless では、仮想の時計で作る。`hw_clock` と `hw_completion` は付けない。
- 表面が複数の出力にまたがるときは、1 つの出力を選ぶ。選んだ出力は、なるべく変えない（XML の勧め）。
- frame callback（`wl_surface.frame`）とは別物である。frame callback は「次を描いてよい」という合図で、
  presentation-time は「前のものがいつ出たか」の報告である。

## 5. 出典

確認日はすべて 2026-11-12。

- wayland-protocols 1.50（`3eab2dccf4bf`）: `stable/xdg-shell/xdg-shell.xml`、
  `unstable/xdg-decoration/xdg-decoration-unstable-v1.xml`、`staging/fractional-scale/fractional-scale-v1.xml`、
  `stable/viewporter/viewporter.xml`、`stable/presentation-time/presentation-time.xml`
  https://gitlab.freedesktop.org/wayland/wayland-protocols
- Wayland の core のプロトコル（wayland 1.26.0 の `wayland.xml`）:
  `wl_surface.preferred_buffer_scale`
- Smithay（`8fd9fe0c74e5`）: `src/wayland/shell/xdg/`、`src/wayland/presentation/`、`src/wayland/viewporter/`
  https://github.com/Smithay/smithay
