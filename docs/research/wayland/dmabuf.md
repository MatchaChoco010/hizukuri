# linux-dmabuf と linux-drm-syncobj

- 確認した版: wayland-protocols 1.50（`3eab2dccf4bf`）、Smithay（`8fd9fe0c74e5`）、Mesa 26.2.6

| プロトコル | interface の版 | Smithay（`8fd9fe0c`）が公開する版 |
| --- | --- | --- |
| linux-dmabuf-v1（stable） | `zwp_linux_dmabuf_v1` version 6 | feedback ありで 6、なしで 3 |
| linux-drm-syncobj-v1（staging） | `wp_linux_drm_syncobj_manager_v1` version 1 | 1 |

## 1. 目的と全体像

**linux-dmabuf** は、クライアントが GPU で描いたバッファを、コピーせずに compositor へ渡すためのプロトコルである。

- クライアントは、dma-buf の fd と、その中身の説明（format、modifier、plane ごとの offset と stride）を渡す。
- compositor は、それを自分の GPU に取り込んで、テクスチャとして使う。あるいは、そのまま表示装置へ出す（direct scanout）。
- compositor は、「どの format と modifier を、どのデバイスで受け取れるか」を、クライアントに知らせる（feedback）。

**linux-drm-syncobj** は、「クライアントの描画が終わるのを、compositor がどう待つか」と、
「compositor がバッファを使い終わったことを、クライアントがどう知るか」を、明示的に取り決める（explicit sync）。

Mesa の Vulkan の WSI と EGL は、どちらのプロトコルも内部で使う。
GPU で描くクライアントを表示するには、linux-dmabuf が要る。

## 2. linux-dmabuf

### 登場する object

| interface | 役割 |
| --- | --- |
| `zwp_linux_dmabuf_v1` | global。バッファを作る入口と、feedback を得る入口 |
| `zwp_linux_buffer_params_v1` | 1 つのバッファを作るための、一時的な object。plane を集める |
| `zwp_linux_dmabuf_feedback_v1` | compositor が受け取れる format、modifier、デバイスの知らせ |
| `wl_buffer` | 作った結果。ふつうのバッファとして、表面に付ける |

### バッファを作る流れ

1. クライアントが、`create_params` で params を作る。
2. plane ごとに、`add`（fd、plane の番号、offset、stride、modifier）を呼ぶ。
3. `create`（幅、高さ、format、flags）か、`create_immed`（version 2 から）を呼ぶ。
   - `create`: compositor が取り込みを試し、成功なら `created`（新しい `wl_buffer`）、失敗なら `failed` を送る。
   - `create_immed`: クライアントが `wl_buffer` の id を先に決める。成功なら、何も送らない。
     失敗なら、プロトコルエラーにするか、無効な `wl_buffer` を作って `failed` を送るかを、compositor が選ぶ。
4. params は、1 度しか使えない（`already_used`）。

**エラー。**

| エラー | 条件 |
| --- | --- |
| `plane_idx` | plane の番号が大きすぎる |
| `plane_set` | 同じ plane を 2 度設定した |
| `incomplete` | format に必要な plane がそろっていない |
| `invalid_format` | format が正しくない。version 4 からは、公開していない format と modifier の組にも使う。version 5 からは、plane ごとに modifier が違う場合にも使う |
| `invalid_dimensions` | 幅か高さが正でない |
| `out_of_bounds` | offset や stride が、dma-buf の大きさに収まらない |
| `invalid_wl_buffer` | `create_immed` で、原因がはっきりしない失敗 |
| `invalid_dev_t_size` | `set_sampling_device` の引数の大きさが違う（version 6） |

**flags。** `y_invert`（上下が逆）、`interlaced`、`bottom_first`。

### modifier

- format（DRM の fourcc）は、画素の並びを表す。
- modifier は、メモリー上の配置（tiling、圧縮など）を表す。driver ごとに違う、64 bit の値である。
- `DRM_FORMAT_MOD_INVALID` は、「modifier を明示しない」という意味である（implicit modifier）。
  配置は、dma-buf から driver が判断する。古い仕組みとの互換のために残っている。
- `DRM_FORMAT_MOD_LINEAR` は、何の工夫もない、行ごとの並びである。どのデバイスでも読めるが、遅いことがある。
- compositor が、ある format について、明示的な modifier と `DRM_FORMAT_MOD_INVALID` の両方を公開したら、
  両方の方式に対応している、という意味になる。

### format と modifier の知らせ方

版によって、2 つの方式がある。

**version 3 まで: イベントで並べる。**

- bind の直後に、`format`（version 1 から）と `modifier`（version 3 から）を、組ごとに送る。
- version 4 で、どちらも非推奨になった。version 4 以上で bind したクライアントには、送らない。

**version 4 から: feedback。**

- クライアントが、`get_default_feedback` か、`get_surface_feedback`（表面）で、feedback の object を作る。
- compositor が、次の順でイベントを送る。
  1. `format_table`（fd、大きさ）: format と modifier の組の表。
  2. `main_device`（version 5 まで）: compositor が主に使うデバイス。
  3. **tranche** を、好ましい順に並べる。tranche ごとに、
     `tranche_target_device`、`tranche_flags`、`tranche_formats`（1 つ以上）、`tranche_done` を送る。
  4. `done`
- 内容が変わったら、全部を送り直す。同じ内容を、続けて送ってはならない。

**format table。**

- fd を mmap して読む。クライアントは、読み取り専用、private で mmap する。
- 1 組が 16 バイトである。format（32 bit）、詰め物（4 バイト）、modifier（64 bit）。バイト順は、その機械のものである。
- compositor は、送った後で、表の中身を書き換えてはならない。変えるときは、新しいファイルを作って、送り直す。
- `tranche_formats` は、この表の中の番号（16 bit）の配列である。

**tranche。**

- 「このデバイス向けに、この format と modifier で作ってくれると、こういう利点がある」という、ひとまとまりの知らせである。
- flag が 2 つある。

| flag | 入った版 | 意味 |
| --- | --- | --- |
| `scanout` | 4 | そのデバイスで、direct scanout を試みるかもしれない |
| `sampling` | 6 | そのデバイスに取り込めば、効率よくテクスチャとして読める |

- デバイスは、`dev_t` を配列として渡す。DRM の node の種類（primary か render か）は、決まっていない。
  クライアントは、`dev_t` の値を比べるだけでは、同じデバイスかどうかを判定できない。

**表面ごとの feedback。**

- `get_surface_feedback` は、その表面についての知らせである。状況によって、内容が変わる。
- 典型的な使い方: 表面が全画面になり、direct scanout できそうになったら、
  `scanout` の flag を付けた tranche を、先頭に足して送り直す。
  クライアントは、scanout できる modifier でバッファを作り直す。

### 版ごとの違い

| 版 | 入ったもの |
| --- | --- |
| 1 | `create_params`、`format` |
| 2 | `create_immed` |
| 3 | `modifier` |
| 4 | feedback（`get_default_feedback`、`get_surface_feedback`）。`format` と `modifier` の非推奨。公開していない組への `invalid_format` |
| 5 | plane ごとに modifier が違う場合の `invalid_format` |
| 6 | `set_sampling_device`。tranche の `sampling` の flag。`main_device` の非推奨 |

**version 6 の変更。** 複数の GPU を、より正確に扱うための変更である。

- `main_device` は、version 6 以上で bind したクライアントには送らない。
- 代わりに、**`sampling` の flag を付けた tranche を、少なくとも 1 つ送らなければならない。**
- クライアントは、`set_sampling_device` で、「このバッファは、このデバイスに取り込んでほしい」と伝えられる。
  伝えなければ、compositor は、対応しているすべてのデバイスで取り込みを試す。

Mesa 26.2.6 の Vulkan の WSI は、version 3 以上の global に bind し、
version は「default feedback が入った版」（4）までに抑えている（`wsi_common_wayland.c`）。

### compositor が守るべき決まり

1. bind された版に合った方式で、format と modifier を知らせる。
2. 公開していない組でのバッファの作成を断る（version 4 から）。
3. 取り込めないバッファは、`failed` で断る。**クライアントを、理由なく切断しない。**
4. format table を、送った後で書き換えない。
5. feedback の内容が変わったら、全部を送り直して `done` で締める。変わっていないのに送り直さない。
6. version 6 以上のクライアントには、`sampling` の tranche を少なくとも 1 つ送る。`main_device` は送らない。
7. dma-buf の fd は、取り込みが終わったら、必要以上に持ち続けない（fd の数の上限に当たる）。

### 落とし穴

- **公開する format と modifier は、実際に取り込めるものだけにする。**
  Vulkan で取り込めるかどうかを、デバイスに問い合わせて確かめた組だけを、表に入れる。
- **driver が「対応している」と言う modifier が、実際には使えないことがある。**
  kernel が plane の `IN_FORMATS` に載せている modifier でも、そのバッファを plane に載せる commit を、driver が拒否する例がある。
  `IN_FORMATS` に載っていることは、commit が通ることを保証しない。
- **implicit modifier のバッファ。** `DRM_FORMAT_MOD_INVALID` のバッファを Vulkan に取り込むのは、難しい。
  Vulkan の `VK_EXT_image_drm_format_modifier` は、明示的な modifier を前提にしている。
  公開するかどうかを、慎重に決める。
- **複数の plane と、複数の fd。** YCbCr の format は、plane ごとに別の fd で届くことがある。
- **バッファの寿命。** GPU がバッファを読んでいる間に、クライアントがバッファを書き換えると、表示が乱れる。
  同期の仕組み（次の節）で防ぐ。
- **取り込みは、表面への commit のたびに行わない。** `wl_buffer` ごとに 1 度だけ取り込み、結果を覚えておく。
  クライアントは、ふつう数枚のバッファを使い回す。

## 3. 同期: implicit sync と linux-drm-syncobj

### 何が問題か

クライアントは、GPU に描画を指示した直後に、バッファを compositor へ渡す。
その時点で、描画はまだ終わっていないことがある。compositor は、描画が終わるのを待ってから、読まなければならない。

反対に、compositor がバッファを読み終わる前に、クライアントが次の描画を始めてもいけない。

### implicit sync

- 待つべき処理の情報（fence）を、dma-buf そのものに、kernel が付けておく方式である。
- compositor は、何もしなくても、driver が待ってくれる。古くからの方式である。
- Vulkan は、この仕組みを自動では使わない。compositor が、自分でつなぐ必要がある。
  - dma-buf から、fence を sync file として取り出す（`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`）。
  - それを、Vulkan の semaphore に取り込んで、描画の前に待つ。
  - 自分の描画が終わることを表す fence を、sync file として取り出し、dma-buf に付ける（`DMA_BUF_IOCTL_IMPORT_SYNC_FILE`）。

### linux-drm-syncobj（explicit sync）

fence を、プロトコルで明示的にやり取りする方式である。DRM の syncobj の timeline を使う。

**登場する object。**

| interface | 役割 |
| --- | --- |
| `wp_linux_drm_syncobj_manager_v1` | global |
| `wp_linux_drm_syncobj_timeline_v1` | クライアントが渡した、syncobj の timeline |
| `wp_linux_drm_syncobj_surface_v1` | 表面ごとの拡張。表面につき 1 つ（`surface_exists`） |

**流れ。**

1. クライアントが、`import_timeline`（fd）で、syncobj の timeline を compositor に渡す。
   取り込めなければ、`invalid_timeline` のエラーになる。
2. クライアントが、commit のたびに、2 つの点を設定する。
   - `set_acquire_point`（timeline、点）: **この点が signal されるまで、compositor はバッファを読んではならない。**
     クライアントの描画が終わると、signal される。
   - `set_release_point`（timeline、点）: **compositor が、バッファを使い終わったら、この点を signal する。**
3. どちらも double-buffered で、`wl_surface.commit` で有効になる。その commit で付けたバッファにだけ効く。

**決まり。**

- バッファを付けた commit では、**両方の点を設定しなければならない。** 片方だけ、あるいはバッファなしで点だけ、はエラーである
  （`no_acquire_point`、`no_release_point`、`no_buffer`）。
- acquire と release が同じ timeline なら、acquire の点は、release の点より小さくなければならない（`conflicting_points`）。
- explicit sync に対応していない種類のバッファを付けると、`unsupported_buffer` のエラーになる。
  linux-dmabuf で作ったバッファは、必ず対応していなければならない。
- この拡張の object がある間、compositor は、その表面のバッファの implicit sync を無視してよい。
- この拡張の object がある間、`wl_buffer.release` が届くかどうかは、未定義になる。
  クライアントは、release の点で、バッファの再利用の可否を判断する。
- compositor は、バッファを 1 度も読まずに release してもよい。release の点が、acquire の点より先に signal されることもある。
- compositor は、release の点を、commit された順とは違う順で signal することがある。
  timeline の点を signal すると、それより前の点もすべて signal される。
  そのため、クライアントは、バッファごとに別の timeline を使うことが、強く勧められている。

**EGL や Vulkan を使うクライアント。** WSI が、この拡張を内部で使う。
アプリが、同じ表面に自分でこの拡張を使うと、`surface_exists` のエラーになる。

### Smithay の実装

Smithay の `wayland::drm_syncobj` の説明による。

- acquire の点は、「表面の transaction が準備できた時点で、すでに signal されている」という前提で扱う。
  そのために、点が signal されるまで commit を保留する仕組み（blocker）を使う。
- この仕組みは、syncobj の eventfd（kernel の機能）に依存する。
  対応しているかを調べる関数があり、対応しているときだけ、プロトコルを公開する。
- release の点は、Smithay の `Buffer` への参照がすべて無くなったときに、signal される。

release の時機は、「Smithay のバッファの参照を、いつ手放すか」で決まる。
レンダラーは、GPU がそのテクスチャを使い終わるまで、参照を持ち続ける必要がある。

## 4. Vulkan への取り込みと、必要な拡張

dma-buf を Vulkan の画像として取り込むのに使う拡張と、プロトコルの項目の対応である。

| やりたいこと | Vulkan の拡張 | プロトコルの項目との対応 |
| --- | --- | --- |
| dma-buf の fd を、メモリーとして取り込む | `VK_KHR_external_memory_fd`、`VK_EXT_external_memory_dma_buf` | `add` の fd |
| modifier を指定して、画像を作る | `VK_EXT_image_drm_format_modifier` | `add` の modifier、offset、stride（plane ごとの配置として渡す） |
| 対応している format と modifier を調べる | 同上（format の性質の問い合わせ） | feedback の format table に載せる組 |
| ほかのプロセスが書いた画像を、受け取る | `VK_EXT_queue_family_foreign` | ― |
| YCbCr の画像を読む | `VK_KHR_sampler_ycbcr_conversion`（Vulkan 1.1 で core） | color-representation の係数、範囲、色差の位置 |
| plane ごとに別のメモリーを結びつける | disjoint な画像（`VK_KHR_bind_memory2`。Vulkan 1.1 で core） | plane ごとに fd が違う場合 |
| fence を sync file でやり取りする | `VK_KHR_external_semaphore_fd`、`VK_KHR_external_fence_fd` | implicit sync の橋渡し、syncobj の点 |
| Vulkan のデバイスが、どの DRM のデバイスかを知る | `VK_EXT_physical_device_drm` | feedback の `main_device`、`tranche_target_device` |

- 取り込める format と modifier の組は、Vulkan に問い合わせて得る。
  用途（テクスチャとして読む）と、画像の大きさの上限も、組ごとに確かめる。
- scanout 用の tranche に載せる組は、KMS の plane の `IN_FORMATS` と、Vulkan で取り込める組の、共通部分である。

**DRM のデバイスが無い環境。**

- feedback に載せる `dev_t` が無い。syncobj も使えない。その場合は、linux-dmabuf を公開せず、`wl_shm` だけで動かす。
- Mesa の WSI は、`wl_shm` にも bind する。lavapipe のような、ソフトウェアの driver で描くクライアントは、`wl_shm` で表示できる。
- lavapipe は、条件が合えば、`VK_EXT_external_memory_dma_buf` と `VK_EXT_image_drm_format_modifier` を公開する。

## 5. 出典

確認日はすべて 2026-11-12。

- wayland-protocols 1.50（`3eab2dccf4bf`）: `stable/linux-dmabuf/linux-dmabuf-v1.xml`、
  `staging/linux-drm-syncobj/linux-drm-syncobj-v1.xml`
  https://gitlab.freedesktop.org/wayland/wayland-protocols
- Smithay（`8fd9fe0c74e5`）: `src/wayland/dmabuf/mod.rs`、`src/wayland/drm_syncobj/mod.rs`
  https://github.com/Smithay/smithay
- Mesa 26.2.6: `src/vulkan/wsi/wsi_common_wayland.c`、`src/gallium/frontends/lavapipe/lvp_device.c`
  https://gitlab.freedesktop.org/mesa/mesa
