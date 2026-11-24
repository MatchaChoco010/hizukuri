# Wayland のプロトコルのまとめ

実装する人が、XML を読む前に、全体をつかむための文書である。
細かい決まりは、必ず XML（と、あれば規範の付録）で確かめること。

各ファイルは、目的、登場する object と状態の流れ、compositor が守るべき決まり、落とし穴、版ごとの違い、出典を書いている。
プロトコルのほかに、実装が直接関わる、外部のものの動作（Smithay、Mesa の Vulkan の WSI、Linux kernel の KMS、fcitx5、xwayland-satellite）も、
版を示して書いている。

| ファイル | 扱うもの |
| --- | --- |
| [color-management.md](color-management.md) | color-management-v1、color-representation-v1。合成の考え方、Mesa の WSI との対応、KMS への伝え方 |
| [ime.md](ime.md) | text-input-v3、input-method-v2、virtual-keyboard-v1。fcitx5 との間の流れ |
| [xdg-shell.md](xdg-shell.md) | xdg-shell、xdg-decoration、fractional-scale、viewporter、presentation-time |
| [dmabuf.md](dmabuf.md) | linux-dmabuf、linux-drm-syncobj。Vulkan への取り込みに必要な拡張 |
| [gaming.md](gaming.md) | tearing-control、KMS の async page flip、VRR、pointer-constraints、relative-pointer、keyboard-shortcuts-inhibit |
| [xwayland-satellite.md](xwayland-satellite.md) | xwayland-satellite が compositor に求めるものと、起動の取り決め |

## 確認した版

2026-11-12 に確認した。各ファイルの出典に、リポジトリごとの commit を書いている。

| もの | 版 |
| --- | --- |
| wayland-protocols | 1.50（`3eab2dccf4bf`、2026-10-29） |
| wayland-rs | `20445fb2ed7d`（2026-11-09）。`wayland-protocols` crate 0.32.13 は、upstream の 1.49 までを取り込んでいる |
| Smithay | `8fd9fe0c74e5`（2026-11-11）。`Cargo.toml` が固定している rev である |
| Mesa | 26.2.6 |
| Linux kernel | 7.4-rc2 |

## 読み方の注意

- hizukuri のコアは、組み込みのレンダラーも、組み込みのウィンドウ管理も持たない（[現時点の方針](../../design/direction.md)）。
  配置、描画、条件の判断のように、プロトコルが compositor に任せていることは、コアが決めるのではなく、
  コアが、スクリプトの層に、決める手段を与える。
- upstream の XML が、`wayland-protocols` crate に入っている版より、新しいことがある。
  実装で使えるのは、crate に入っている版までである。
- Smithay の rev や、依存の版を上げたときは、該当するファイルの記述を、確かめ直す。
