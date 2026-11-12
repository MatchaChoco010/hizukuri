# IME: text-input-v3、input-method-v2、virtual-keyboard-v1

- 確認した版:
  - `text-input-unstable-v3`: wayland-protocols 1.50。interface は version 2（1.49 でも version 2）
  - `input-method-unstable-v2`: `wayland-protocols-misc` crate 0.3.12 に入っている XML。interface は version 1
  - `virtual-keyboard-unstable-v1`: 同じ crate の XML。interface は version 1
  - Smithay（`8fd9fe0c74e5`）、fcitx5（`5ffd9e5`、2026-11-04。7 節の後半は 5.1.23）

## 1. 目的と全体像

3 つのプロトコルは、役割が違う。compositor は、その間に立つ。

| プロトコル | 相手 | 役割 |
| --- | --- | --- |
| text-input-v3 | アプリ | アプリが「ここに文字を入れたい」と伝える。compositor が、変換中の文字列と確定した文字列を送る |
| input-method-v2 | IME（fcitx5 など） | IME が、アプリの入力欄の状態を受け取り、変換中の文字列と確定した文字列を送る。キーボードを grab する |
| virtual-keyboard-v1 | IME | IME が、自分で処理しなかったキーを、キー入力として送り返す |

```text
アプリ ── text-input-v3 ── compositor ── input-method-v2 ──── IME
                               │      └─ virtual-keyboard-v1 ─┘
                            wl_keyboard
                               │
                             アプリ
```

- input-method-v2 は、upstream の wayland-protocols には入っていない。
  wayland-protocols にあるのは、古い input-method-unstable-v1 だけである。
- text-input-v3 と input-method-v2 は、ほぼ鏡写しになっている。
  compositor の仕事の大半は、片方の状態を、もう片方へ写すことである。

## 2. 登場する object

**text-input-v3（アプリ側）**

| interface | 役割 |
| --- | --- |
| `zwp_text_input_manager_v3` | global。seat ごとの text-input を作る |
| `zwp_text_input_v3` | seat の text-input。アプリは、1 つの seat にいくつ作ってもよい |

**input-method-v2（IME 側）**

| interface | 役割 |
| --- | --- |
| `zwp_input_method_manager_v2` | global。seat の input method になる |
| `zwp_input_method_v2` | seat の input method。**seat につき 1 つだけ** |
| `zwp_input_popup_surface_v2` | 候補ウィンドウの表面。`wl_surface` に「input_popup」の role を与える |
| `zwp_input_method_keyboard_grab_v2` | キーボードの grab。`wl_keyboard` と同じ形のイベントを受け取る |

**virtual-keyboard-v1（IME 側）**

| interface | 役割 |
| --- | --- |
| `zwp_virtual_keyboard_manager_v1` | global。seat の仮想キーボードを作る |
| `zwp_virtual_keyboard_v1` | 仮想キーボード。keymap、キー、modifier を送る |

## 3. text-input-v3 の状態の流れ

### focus

- text-input の focus は、キーボードの focus に従う。
- compositor は、focus のある表面のクライアントが持つ、**すべての** `zwp_text_input_v3` に `enter` を送る。
- focus が外れると `leave` を送る。`leave` は、次の `enter` より先に送る。
- `leave` の後、次の `enter` まで、text-input からの要求は無視しなければならない。

### 有効と無効

- アプリは、入力欄に focus が入ったら `enable`、外れたら `disable` を送る。
- `enable` と `disable` は、対になっているとは限らない。同じ要求が続いても、compositor は扱えなければならない。
- 1 つの seat で有効にできる text-input は 1 つである。
  ほかが有効なときの `enable` は、compositor が無視しなければならない。
- `enable` は、それまでの状態をすべて消す。アプリは、続けて状態を送り直す。

### 状態と commit

アプリが送る状態は、次のものである。すべて double-buffered で、`commit` で有効になる。

| 要求 | 内容 |
| --- | --- |
| `set_surrounding_text` | カーソルの前後の文字列、カーソルの位置、選択の始点。位置はバイト単位。4000 バイトまで |
| `set_text_change_cause` | 文字列が変わった理由（IME によるものか、そうでないか）。commit のたびに既定値に戻る |
| `set_content_type` | 入力欄の用途（`content_purpose`）と、振る舞いの hint（`content_hint`） |
| `set_cursor_rectangle` | カーソルの周りの矩形。表面の座標。候補ウィンドウの位置決めに使う |
| `set_available_actions`（version 2 から） | 入力欄が受け付ける action |

- `set_surrounding_text` と `set_cursor_rectangle` を 1 度も送らないアプリは、その機能に対応していない、という意味になる。
- **compositor は、text-input ごとに `commit` の回数を数える。** その回数を、`done` の serial に使う。

### compositor からアプリへ

| イベント | 内容 |
| --- | --- |
| `preedit_string` | 変換中の文字列と、その中のカーソルの範囲（バイト単位）。両方が -1 ならカーソルを隠す |
| `commit_string` | 確定した文字列 |
| `delete_surrounding_text` | カーソルの前後から消すバイト数 |
| `action`（version 2 から） | 入力欄への action（`submit`） |
| `preedit_hint`（version 2 から） | 変換中の文字列の、部分ごとの見た目の hint |
| `language`（version 2 から） | IME が使っている言語（BCP 47） |
| `done` | ここまでの変更をまとめて適用する。serial を運ぶ |

- これらも double-buffered である。`done` で、まとめて適用される。
- アプリは、`done` を受け取ったら、決まった順で適用する。
  1. 今の変換中の文字列を、カーソルに置き換える。
  2. 指定された前後の文字列を消す。
  3. 確定した文字列を挿入し、カーソルをその末尾に置く。
  4. 送るべき surrounding text を計算する。
  5. 新しい変換中の文字列を、カーソルの位置に挿入する。
  6. カーソルを、変換中の文字列の中に置く。
  7. action を実行する（version 2 から）。

### serial の意味

- `done` の serial は、「compositor が知っている、アプリの最後の状態」を表す。値は、それまでに受け取った `commit` の回数である。
- アプリは、serial が自分の `commit` の回数と一致しないとき、変更は適用するが、自分の状態（surrounding text など）は送り直さない。
  古い状態を元にした変更だからである。
- これは、アプリと IME が同時に状態を変えたときの食い違いを収める仕組みである。

## 4. input-method-v2 の状態の流れ

### 有効と無効

- IME は、`get_input_method` で seat の input method になる。
- **seat につき 1 つだけである。** すでにあるときに作られた object には、`unavailable` だけを送る。
- アプリが text-input を有効にする（`enable` を commit する）と、compositor は IME に `activate` を送る。
  無効にすると `deactivate` を送る。
- `activate` は、それまでの状態をすべて消す。

### compositor から IME へ

text-input の状態を、そのまま写す。

| イベント | 元になる text-input の要求 |
| --- | --- |
| `activate`、`deactivate` | `enable`、`disable` |
| `surrounding_text` | `set_surrounding_text` |
| `text_change_cause` | `set_text_change_cause` |
| `content_type` | `set_content_type` |
| `done` | `commit` |

- これらは double-buffered で、`done` で適用される。
- **IME は、`done` の回数を数える。** その回数を、自分の `commit` の serial に使う。

### IME から compositor へ

| 要求 | 写す先の text-input のイベント |
| --- | --- |
| `set_preedit_string` | `preedit_string` |
| `commit_string` | `commit_string` |
| `delete_surrounding_text` | `delete_surrounding_text` |
| `commit(serial)` | `done(serial)` |

- IME の `commit` の serial は、IME が受け取った `done` の回数である。
- compositor は、serial が自分の送った `done` の回数と一致しないとき、処理は進めるが、input method の状態は変えない。
- 無効の状態で送られた要求も、compositor は受け付けなければならない。serial の仕組みがあるので、次の入力欄には影響しない。

### 2 つの serial の対応

compositor は、2 つの数え方を取り違えないようにする。

| 向き | 数えるもの | 使うところ |
| --- | --- | --- |
| アプリ → compositor | text-input の `commit` の回数 | compositor が text-input に送る `done` の serial |
| compositor → IME | input method に送った `done` の回数 | IME が送る `commit` の serial の検査 |

## 5. キーボードの grab と仮想キーボード

### grab

- IME は、`grab_keyboard` で、seat のキーボードを grab する。
- grab している間、compositor は、キーボードのイベントを、アプリではなく grab の object へ送る。
  keymap、キー、modifier、repeat の設定が届く。
- **compositor は、grab へ送ったイベントを、それ以上処理してはならない。**
  アプリには送らない。
- compositor は、特定のイベントを grab へ送らない、と決めてもよい。
  compositor 自身のキーバインドは、grab より先に処理する。
- grab の object を解放すると、grab が外れる。

### 仮想キーボード

- IME は、grab で受け取ったキーのうち、自分で処理しなかったものを、仮想キーボードで送り返す。
  たとえば、変換中でないときの矢印キーや、ショートカットである。
- 仮想キーボードは、最初に keymap を設定する。keymap が無いままキーを送ると、`no_keymap` のエラーになる。
- compositor は、仮想キーボードのキーを、物理キーボードのキーと同じようにアプリへ届ける。
- **仮想キーボードのキーを、もう 1 度 grab へ送ってはいけない。** 送ると、IME との間でキーが回り続ける。
- 仮想キーボードは、任意のキー入力を作れる。信頼できないクライアントには、`unauthorized` のエラーを返すことが勧められている。

### Smithay での仮想キーボードの扱い

Smithay（`8fd9fe0c`）の仮想キーボードは、次のように動く。

- 仮想キーボードは、自分ではアプリへキーを送らない。**入力デバイスの 1 つ**として扱われる。
- compositor は、仮想キーボードのイベントを受け取り、実デバイスのイベントと同じ経路で処理する。
- 処理の前に、そのデバイスの keymap を有効にする必要がある。
- デバイスが消えたときの modifier の後始末も、compositor の仕事である。

この経路で、「仮想キーボードのキーを grab へ送らない」ことを、compositor が自分で保証する必要がある。

- grab を持つ IME のクライアントか、どうかは、`InputMethodHandle::keyboard_grab_client` で分かる。
  そのキーは、`KeyboardHandle::input_forward_bypassing_grab` で、grab を通さずに focus へ送れる。
  この関数は、xkb の状態を更新しない。
- `input_forward_bypassing_grab` の `mods_changed` を `true` にすると、modifier は、キーの**後**に送られる
  （`KeyboardInnerHandle::input` が、key、modifiers の順に送る）。関数の文書の「前に送る」という説明と、実装が食い違っている。
- 仮想キーボードが消えると、押されたままのキーの「離す」を `Keyboard` のイベントとして送り、その後に `DeviceRemoved` を送る。
  IME のクライアントが落ちたときは、その時点で、grab も `keyboard_grab_client` も無くなっていることがある。

## 6. 候補ウィンドウの位置決め

- IME は、`get_input_popup_surface` で、候補ウィンドウの表面を作る。
  表面にすでに role があれば、プロトコルエラーにする。
- この表面は、**input method が有効なときだけ見える**。`deactivate` で、compositor が隠す。
- 位置を決めるのは compositor である。IME は位置を指定できない。
- compositor は、アプリが `set_cursor_rectangle` で伝えた矩形の近くに、候補ウィンドウを置く。
  矩形はアプリの表面の座標なので、アプリの表面の位置を足して、画面上の位置にする。
- compositor は、`text_input_rectangle` で、入力位置の矩形を IME に伝える。座標は、候補ウィンドウの表面から見たものである。
  IME は、これを使って、候補ウィンドウの中の配置（どちら向きに伸ばすか、など）を決める。
- 画面の端では、候補ウィンドウがはみ出さないように、位置をずらす。カーソルの矩形を隠さないようにする。

Smithay（`8fd9fe0c`）は、アプリが `set_cursor_rectangle` で伝えた矩形を、アプリの表面の座標のまま、
`text_input_rectangle` で送る。プロトコルの「surface local coordinates」を、候補ウィンドウの表面から見た座標として送るには、
compositor が、候補ウィンドウを置いた後に、送り直す必要がある。

**text-input-v3 の version 2 での変更。**

- version 1 では、`set_cursor_rectangle` は text-input の `commit` で有効になる。
- version 2 では、text-input の `commit` では「確定待ち」に入るだけで、
  focus のある表面の次の `wl_surface.commit` で有効になる。
- 狙いは、候補ウィンドウの位置を、アプリの表示の更新と同じフレームで動かすことである。

**ウィンドウを変形して表示する場合。** 候補ウィンドウは、アプリの表面の、見た目の位置に付いて動く必要がある。
カーソルの矩形は、表面の座標で届く。画面上の位置にするには、表面の、見た目の配置を知っている必要がある。

## 7. fcitx5 との間で流れるもの

fcitx5（`5ffd9e5`）の Wayland の frontend（`src/frontend/waylandim/waylandimserverv2.cpp`）の動作である。
fcitx5 は、`zwp_input_method_manager_v2` と `zwp_virtual_keyboard_manager_v1` の**両方**があるときに、この経路を使う。

日本語を入力して確定するまでを、順に書く。

**準備**

1. fcitx5 が起動し、2 つの global に bind する。`get_input_method` で、seat の input method になる。

**入力欄に focus が入る**

2. ユーザーが、アプリの入力欄をクリックする。compositor が、キーボードの focus をアプリの表面に移す。
3. compositor が、アプリの text-input に `enter` を送る。
4. アプリが `enable`、`set_surrounding_text`、`set_content_type`、`set_cursor_rectangle` を送り、`commit` する。
5. compositor が、fcitx5 に `activate`、`surrounding_text`、`content_type`、`done` を送る。
6. fcitx5 は、`done` を受け取ったところで、次のことをする。
   - 仮想キーボードを作り直す（設定によっては、使い回す）。
   - 古い grab を解放し、`grab_keyboard` で新しく grab する。
7. compositor が、grab の object に `keymap` と `repeat_info` を送る。
8. fcitx5 は、受け取った keymap を、仮想キーボードにも設定する。

**キーを打つ**

9. ユーザーが「k」を押す。compositor は、キーを、アプリではなく grab の object に送る。
10. fcitx5 が、キーを処理する。変換中の文字列「ｋ」ができる。
11. fcitx5 が `set_preedit_string` と `commit(serial)` を送る。
12. compositor が、アプリの text-input に `preedit_string` と `done` を送る。アプリが、変換中の文字列を表示する。
13. 「a」を押すと、同じ流れで、変換中の文字列が「か」になる。
14. fcitx5 が、候補ウィンドウの表面を作り、内容を描く。
    compositor が、アプリのカーソルの矩形の近くに置き、`text_input_rectangle` を送る。
15. アプリは、変換中の文字列を表示した結果、カーソルの矩形が変わったら、`set_cursor_rectangle` と `commit` を送る。
    compositor は、候補ウィンドウを動かす。

**確定する**

16. ユーザーが Enter を押す。compositor は、キーを grab に送る。
17. fcitx5 が `commit_string("か")` と `commit(serial)` を送る。変換中の文字列は、空になる。
18. compositor が、アプリに `commit_string` と `done` を送る。アプリが、文字を挿入する。
19. アプリが、新しい surrounding text とカーソルの矩形を送り、`commit` する。compositor が、それを fcitx5 に写す。

**IME が処理しないキー**

20. 変換中でないときに、ユーザーが矢印キーを押す。compositor は、キーを grab に送る。
21. fcitx5 は、処理しないと判断し、仮想キーボードで同じキーを送る。modifier の変化も、仮想キーボードで送る。
22. compositor は、仮想キーボードのキーを、`wl_keyboard` のイベントとして、アプリへ届ける。

**focus が外れる**

23. focus がほかへ移る。アプリが `disable` と `commit` を送る（または、compositor が `leave` を送る）。
24. compositor が、fcitx5 に `deactivate` と `done` を送る。候補ウィンドウを隠す。
25. fcitx5 は、仮想キーボードで押したままのキーがあれば、離すイベントを送る。grab を解放する。

fcitx5 5.1.23 の同じファイルから、ほかに分かること:

- grab で受け取った `modifiers` を、仮想キーボードの `modifiers` で、そのまま送り返す。
- キーの繰り返しを、自分で作る。押したキーの繰り返しの時機に、仮想キーボードで「離す」と「押す」を送り直す
  （`repeat_info` の delay より、少し早く始める）。
- `done` を受け取るたびに、変化が無くても、preedit を送り直して `commit` する（`done` と `commit` を対にしたがる compositor のため）。
- 仮想キーボードは、既定では、activate のたびに作り直す（設定の `PersistentVirtualKeyboard` で、使い回す）。
- 処理しなかったキーの一部は、仮想キーボードのキーではなく、確定した文字列として送ることがある。

fcitx5 のソースには、「grab は 1 つしか持てない。2 つのクライアントの間を切り替えると `activate` が 2 度続くことがあり、
そのとき 2 つ目の grab の要求が失敗して、無効な object になる」という注記がある。
compositor は、`activate` を重ねて送らないようにし、grab の要求が重なったときにも落ちないようにする。

## 8. compositor が守るべき決まり

1. text-input の focus を、キーボードの focus に合わせる。`leave` を `enter` より先に送る。
2. クライアントが持つ、すべての text-input に `enter` を送る。
3. `leave` の後、次の `enter` までの text-input の要求を無視する。
4. 1 つの seat で、有効な text-input は 1 つにする。重なる `enable` は無視する。
5. text-input の `commit` の回数を数え、`done` の serial に使う。
6. input method は、seat につき 1 つにする。2 つ目には `unavailable` を送る。
7. input method に送った `done` の回数を数え、IME の `commit` の serial と比べる。
8. 無効な状態の input method からの要求も、エラーにせず受け付ける。
9. grab へ送ったキーを、ほかへ送らない。
10. 仮想キーボードのキーを、grab へ戻さない。
11. 候補ウィンドウは、input method が有効なときだけ見せる。
12. 文字列の位置と長さは、バイト単位で扱う。UTF-8 の文字の途中を指してはならない。
13. `set_available_actions` に `none` や重複が入っていたら、`invalid_action` のエラーにする（version 2）。

## 9. 落とし穴

- **compositor 自身のキーバインド。** grab より先に処理する。Super キーの組み合わせなどが、IME に吸われないようにする。
- **キーの繰り返し。** grab している間、繰り返しは IME の側で行われる（`repeat_info` を送るのはそのためである）。
  compositor が、繰り返しのキーを二重に作らないようにする。
- **押したままのキー。** grab が始まる前に押されたキーと、grab が外れた後に離されるキーの扱いに気を付ける。
  アプリから見て、押したのに離されないキーが残らないようにする。
- **IME がいないとき。** Smithay は、input method がいないと、text-input に `enter` を送らない。
  アプリは、text-input が使えないと判断して、ふつうのキー入力で動く。
- **text-input を使わないアプリ。** text-input-v3 に対応していないアプリには、この経路では文字が入らない。
- **候補ウィンドウと popup の重なり。** 候補ウィンドウは xdg_popup ではない。重なりの順は、compositor が決める。
  アプリの popup より手前に出す必要がある。
- **version 2 のカーソルの矩形。** 有効になる時機が、version 1 と違う。Smithay（`8fd9fe0c`）が公開するのは、text-input-v3 も input-method-v2 も、version 1 である。
- **Smithay の text-input は、focus を、キーボードの focus に自動で合わせる。** 候補ウィンドウの位置は、compositor が handler で決める。
  キーボードの focus が動くと、Smithay は、元の focus の leave と、新しい focus の enter のそれぞれで、IME に `deactivate` と `done` を送る
  （有効でなかったときにも送る）。この経路では、`TextInputActivation::deactivated` は呼ばれない。
- **Smithay の候補ウィンドウの親。** 親は、IME が候補ウィンドウを作った時点の、text-input の focus である。
  text-input の focus は、IME が active かどうかに関わらず、キーボードの focus に合わせてあるので、親があることは、
  見せてよいことを意味しない。
- **メニューの grab。** キーボードの grab は 1 つである。IME の grab がある間に、`xdg_popup.grab` のための grab を取ると、
  IME の grab が外れる。Smithay は、外した IME の grab を、後で戻す口を持たない。

## 10. 出典

確認日はすべて 2026-11-12。

- wayland-protocols 1.50（`3eab2dccf4bf`）: `unstable/text-input/text-input-unstable-v3.xml`
  https://gitlab.freedesktop.org/wayland/wayland-protocols
- wayland-rs（`20445fb2ed7d`）: `wayland-protocols-misc/protocols/input-method-unstable-v2.xml`、
  `wayland-protocols-misc/protocols/virtual-keyboard-unstable-v1.xml`
  https://github.com/Smithay/wayland-rs
- fcitx5（`5ffd9e5`、2026-11-04）: `src/frontend/waylandim/waylandimserverv2.cpp`、`src/ui/classic/waylandinputwindow.cpp`
  https://github.com/fcitx/fcitx5
- fcitx5 5.1.23（nixpkgs の `fcitx5` の source）: `src/frontend/waylandim/waylandimserverv2.cpp`
- Smithay（`8fd9fe0c74e5`）: `src/wayland/text_input/`、`src/wayland/input_method/`、`src/wayland/seat/keyboard.rs`、`CHANGELOG.md`
  https://github.com/Smithay/smithay
