# ゲーム向け: tearing-control、async page flip、VRR、ポインターとキーボードの制御

- 確認した版: wayland-protocols 1.50（`3eab2dccf4bf`）、Smithay（`8fd9fe0c74e5`）、Linux kernel 7.4-rc2、Mesa 26.2.6、libinput 1.31.3

| プロトコル | interface の版 | Smithay（`8fd9fe0c`） |
| --- | --- | --- |
| tearing-control-v1（staging） | `wp_tearing_control_manager_v1` version 1 | 実装なし |
| pointer-constraints-unstable-v1 | `zwp_pointer_constraints_v1` version 1 | あり |
| relative-pointer-unstable-v1 | `zwp_relative_pointer_manager_v1` version 1 | あり |
| keyboard-shortcuts-inhibit-unstable-v1 | `zwp_keyboard_shortcuts_inhibit_manager_v1` version 1 | あり |

ゲームを快適に動かすために要るものを、1 つにまとめる。遅延を減らす表示（ティアリング、VRR）と、
入力の制御（ポインターの固定、相対移動、compositor のショートカットの抑止）である。

## 1. 全体像

| やりたいこと | クライアントとの取り決め | compositor が KMS にすること |
| --- | --- | --- |
| 垂直同期を待たずに、すぐ表示する | tearing-control で、表面が希望を伝える | atomic commit に `DRM_MODE_PAGE_FLIP_ASYNC` を付ける |
| フレームができた時点で、表示を更新する | 取り決めは無い。compositor が決める | CRTC の `VRR_ENABLED` を有効にする |
| 視点をマウスで動かす | pointer-constraints の lock と、relative-pointer | ―（入力の配送の話である） |
| Super キーなどを、ゲームに渡す | keyboard-shortcuts-inhibit | ― |

## 2. tearing-control-v1

### 目的と流れ

表面が、「この内容は、ティアリングが起きてもよいので、遅延を小さくしてほしい」と伝える。
**伝えるのは希望だけである。** 実際にどう表示するかは、compositor が決める。

1. クライアントが、`get_tearing_control`（表面）で、表面ごとの object を作る。
   表面につき 1 つである（`tearing_control_exists`）。
2. `set_presentation_hint` で、希望を設定する。double-buffered で、commit で有効になる。

| 値 | 意味 |
| --- | --- |
| `vsync` | 既定。垂直同期に合わせる。ティアリングは起きない |
| `async` | 遅延を最小にする。ティアリングは許す |

3. object を消すと、次の commit で `vsync` に戻る。
4. 表面が消えると、object は inert になる。

### 決まり

- compositor は、ハードウェアの能力、表面の状態、ユーザーの設定に応じて、希望に従っても、無視してもよい。
- EGL や Vulkan を使うクライアントでは、WSI がこの拡張を内部で使う。
  Mesa 26.2.6 の Vulkan の WSI は、compositor がこの global を公開しているときだけ、
  `VK_PRESENT_MODE_IMMEDIATE_KHR` をアプリに見せる。その present mode の swapchain で、`async` を設定する。

## 3. KMS の async page flip

### 仕組み

- ふつうの page flip は、次の垂直帰線の期間に、表示するバッファを切り替える。
- async の page flip は、**待たずに、すぐ切り替える**。走査の途中で切り替わるので、画面の上と下で、違うフレームが見える（ティアリング）。
- atomic commit に、`DRM_MODE_PAGE_FLIP_ASYNC` の flag を付ける。
- driver が atomic の async flip に対応しているかは、`DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP` で調べる。

### 制約

kernel の uAPI の説明（`include/uapi/drm/drm_mode.h`）による。

- ハードウェアが、その更新を async で行えないとき、driver はエラーを返す。
  compositor は、ふつうの flip でやり直すなどして、対処しなければならない。
- ハードウェアによっては、async に切り替える前に、同期の flip を 1 回はさむ必要がある。
  その 1 回は、async でなくても、成功として返る。

kernel の実装（`drivers/gpu/drm/drm_atomic_uapi.c` の `drm_atomic_set_property`）による。

- **async の commit では、ほとんどの property を変えられない。** 値が変わる property を入れると、commit は `EINVAL` で拒否される。
  - connector と CRTC の property は、どれも変えられない。
  - plane の property で変えられるのは、`FB_ID`、`IN_FENCE_FD`、`FB_DAMAGE_CLIPS` だけである。
  - primary 以外の plane は、driver が async の更新に対応しているときだけ、変えられる。
- したがって、カーソルの plane の位置を、async の commit で動かすことは、ふつうはできない。
  ティアリングで出している間は、カーソルを、フレームの中に合成する。
- driver に拒否されたフレームは、同期の flip でやり直す。

### direct scanout との関係

- ティアリングの利点が最も大きいのは、クライアントのバッファを、そのまま primary plane に載せる場合（direct scanout）である。
- 合成を通す場合でも、合成の結果を async で出せば、垂直同期を待つ時間は無くせる。

## 4. VRR（可変リフレッシュレート）

### 仕組み

- 表示装置が、次のフレームが届くまで、垂直帰線の期間を延ばして待つ。
  フレームができた時点で表示が更新されるので、ティアリングなしに、遅延と、かくつきを減らせる。
- connector の `vrr_capable`（読み取り専用）が、表示装置と driver の対応を表す。
- CRTC の `VRR_ENABLED` を有効にすると、driver が VRR を使う。
  有効な間、page flip が来るまで、または上限の時間まで、垂直帰線の期間が延びる（kernel の `drm_connector.c` の説明）。

### 取り決めは無い

- VRR を使うかどうかを、クライアントが伝えるプロトコルは無い。compositor の方針で決める。
- 関係するプロトコルは、presentation-time である。出力のリフレッシュレートが一定でないとき、
  `presented` の `refresh` に何を入れるかが、版で違う（[xdg-shell のまとめ](xdg-shell.md)の 4 節）。

### 落とし穴

- **カーソルだけが動くとき。** VRR では、フレームが来ないと、表示が更新されない。
  ゲームが 40 fps で描いているとき、カーソルの動きも 40 fps に見える、という問題が起きうる。
- **低いフレームレートでのちらつき。** 表示装置によっては、リフレッシュレートが大きく変わると、明るさが変わって見える。
  デスクトップで常に有効にしない方針が多いのは、このためである。
- **compositor の描画の間隔。** VRR では、「次の垂直帰線」が決まっていない。
  frame callback をいつ送るか、アニメーションの時刻をどう進めるかを、決め直す必要がある。
- VRR と async flip は、組み合わせられる。

## 5. pointer-constraints と relative-pointer

一人称視点のゲームは、マウスの動きを、画面上の位置ではなく、視点の回転として使う。
2 つのプロトコルを、組み合わせて使う。

### pointer-constraints

ポインターの動きを制限する。2 種類ある。

| 種類 | 要求 | 効果 |
| --- | --- | --- |
| lock | `lock_pointer` | ポインターを、その場に固定する。`wl_pointer.motion` が届かなくなる |
| confine | `confine_pointer` | ポインターを、領域の中に閉じ込める |

**流れ。**

1. クライアントが、`lock_pointer`（表面、ポインター、領域、寿命）を呼ぶ。
2. 要求は、すぐには有効にならない。compositor が、条件が整ったと判断したときに有効にして、`locked` を送る。
   有効になる時点で、表面にポインターの focus があり、ポインターが領域の中にあることが、保証される。
3. compositor が固定を解くと、`unlocked` を送る。クライアントが object を消しても、固定は解ける。

**決まり。**

- 1 つの表面について、同じ seat で、有効または要求中の制約は 1 つだけである（`already_constrained`）。
- 領域は、表面の input region との共通部分で考える。領域が null なら、input region を使う。
- 寿命は 2 種類ある。

| 寿命 | 意味 |
| --- | --- |
| `oneshot` | 1 度解けたら、二度と有効にならない |
| `persistent` | 解けた後、条件が整えば、また有効になる |

- `set_cursor_position_hint`（lock）: クライアントが自分でカーソルを描いているとき、その位置を伝える。
  compositor は、固定を解くときに、ポインターをその位置へ移してよい。
  移した場合、relative-pointer のイベントは出さない。
- `set_region`: 領域を変える。double-buffered である。
  confine の間に、新しい領域の外にポインターが出たら、compositor は、ポインターを領域の中へ移す（`wl_pointer.motion` を送り、
  relative motion は送らない）か、confine を解いてよい。
- `zwp_confined_pointer_v1` の interface の説明は、「compositor が confine を解いたら、object は defunct」と書く。
  `unconfined` の event の説明は、寿命が `persistent` なら、また有効になりうると書く。寿命の説明と合うのは、後者である。
- **いつ有効にするか、いつ解くかは、compositor が決める。**
  プロトコルは、要求がいつか有効になることを保証していない。

### relative-pointer

- `get_relative_pointer`（ポインター）で、object を作る。
- compositor が、`relative_motion` を送る。

| 引数 | 内容 |
| --- | --- |
| 時刻 | マイクロ秒。64 bit を、上位と下位に分けて送る |
| `dx`、`dy` | 相対的な移動量。ポインターの加速を適用した後の値 |
| `dx_unaccel`、`dy_unaccel` | 加速と、正規化などの変換を適用する前の値。デバイスの生の値そのものという意味ではない |

- 画面の端でポインターが止まっても、相対的な移動量は、止まらずに届く。
- `wl_pointer.motion` と対になっているわけではない。lock している間は、`relative_motion` だけが届く。
- ゲームは、ふつう、加速なしの値を使う。libinput が、両方の値を出す。
  libinput 1.31 の説明（`libinput.h`）では、加速ありの値は「標準化したマウスの画素の動き」、
  加速なしの値は「デバイスの生の座標」で、デバイスの解像度に依存する。
- `relative_motion` は、`wl_pointer` と focus を共有する。ポインターのある表面のクライアントにだけ届く。

### プロトコルが、compositor に任せていること

- **lock をいつ有効にするか。** ふつうは、表面にキーボードの focus があり、ポインターが表面の上にあるときである。
- **lock をいつ解くか。** focus が外れたとき、ワークスペースを切り替えたとき、compositor の画面（一覧など）を出したときである。
- **ユーザーが強制的に解く方法。** クライアントが固定を解かなくても、ユーザーがポインターを取り戻せるようにする。
- **confine の領域の形。** 領域は、矩形の集まりである。四角くない領域の中で、ポインターをどう動かすかを決める。
- confine の領域は、表面の座標で与えられる。ウィンドウを変形して表示する場合は、ポインターの位置を、表面の座標に直してから、制限をかける。
  lock では、相対的な移動量だけを送るので、配置の影響を受けない。

## 6. keyboard-shortcuts-inhibit

### 目的と流れ

表面が、「compositor のショートカットを止めて、すべてのキーを自分に渡してほしい」と求める。
ゲーム、仮想マシンの画面、リモートデスクトップ、nested の compositor が使う。

1. クライアントが、`inhibit_shortcuts`（表面、seat）で、inhibitor を作る。
   同じ表面と seat に、すでにあれば、`already_inhibited` のエラーになる。
2. compositor が、抑止を有効にしたら、`active` を送る。
3. compositor が、ショートカットの処理を元に戻したら、`inactive` を送る。
4. クライアントが inhibitor を消すと、抑止が外れる。

### 決まり

- 抑止が効くのは、その表面にキーボードの focus があるときだけである。
- **compositor は、すべてのショートカットを止める義務はない。**
  ユーザーが、キーボードの扱いを強制的に元に戻すための組み合わせを、残してよい。
- inactive になった inhibitor を、クライアントの側から有効に戻す方法は無い。ユーザーの操作で、戻せるようにしてよい。

### プロトコルが、compositor に任せていること

- 抑止を、無条件に認めるか、ユーザーに確かめるか。
  どのアプリでも、ショートカットを奪えることになるので、信頼の問題がある。
- どの組み合わせを、抑止されないものとして残すか。
- 抑止が有効であることを、ユーザーにどう見せるか。

## 7. 出典

確認日はすべて 2026-11-12。

- wayland-protocols 1.50（`3eab2dccf4bf`）: `staging/tearing-control/tearing-control-v1.xml`、
  `unstable/pointer-constraints/pointer-constraints-unstable-v1.xml`、
  `unstable/relative-pointer/relative-pointer-unstable-v1.xml`、
  `unstable/keyboard-shortcuts-inhibit/keyboard-shortcuts-inhibit-unstable-v1.xml`
  https://gitlab.freedesktop.org/wayland/wayland-protocols
- Linux kernel（7.4-rc2）: `include/uapi/drm/drm.h`（`DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP`）、
  `include/uapi/drm/drm_mode.h`（`DRM_MODE_PAGE_FLIP_ASYNC`）、`drivers/gpu/drm/drm_connector.c`（`VRR_ENABLED`）、
  `drivers/gpu/drm/drm_atomic_uapi.c`（async の commit で変えられる property）
  https://github.com/torvalds/linux
- Mesa 26.2.6: `src/vulkan/wsi/wsi_common_wayland.c` https://gitlab.freedesktop.org/mesa/mesa
- Smithay（`8fd9fe0c74e5`）: `src/wayland/pointer_constraints.rs`、`src/wayland/relative_pointer.rs`、
  `src/wayland/keyboard_shortcuts_inhibit/`
  https://github.com/Smithay/smithay
- libinput 1.31.3: `include/libinput.h`（`libinput_event_pointer_get_dx`、`libinput_event_pointer_get_dx_unaccelerated`）
  https://gitlab.freedesktop.org/libinput/libinput
