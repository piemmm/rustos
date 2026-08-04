# `/System/Fonts` — the shipped font store

One directory per font family, planted verbatim at `/System/Fonts/<key>/` by
the image builder and read by the `fontd` service. A directory is a family
exactly when it holds a `FontFamily` manifest; the service discovers the store
by scanning it, so shipping a family is dropping its directory here — no list
anywhere names a face.

## `FontFamily` manifest

Line-oriented `key = value`, `#` comments, order-significant `face` lines:

| key        | meaning                                                          |
|------------|------------------------------------------------------------------|
| `label`    | the name a user sees in a font picker                             |
| `kind`     | `proportional`, `monospace`, or `fallback` (not user-selectable)  |
| `face`     | a face file in this directory; repeated, **first match wins**     |
| `fallback` | another family directory whose faces extend this one's coverage   |

A scalar resolves to the first `face` whose `cmap` maps it, then to the
`fallback` family's faces in their own order. Coverage is therefore layered by
ordering alone: the primary face owns Latin, and a companion is only ever
reached for what the primary does not map.

## Families

| key             | kind         | covers                                                        |
|-----------------|--------------|---------------------------------------------------------------|
| `inter`         | proportional | the desktop UI face — Latin, Greek, Cyrillic, Vietnamese       |
| `noto-sans`     | proportional | alternative UI sans, widest Latin/Greek/Cyrillic repertoire    |
| `noto-serif`    | proportional | serif alternative for reading text                             |
| `mono`          | monospace    | the terminal and console family, and the console atlas source  |
| `sans-fallback` | fallback     | Hebrew, Chinese, Japanese and Korean for the proportional trio |

`sans-fallback` is shared by all three proportional families, so the 28 MB of
CJK coverage is stored once. `mono` carries its own fixed-pitch companions
because a monospace family needs fixed-pitch CJK to keep the terminal grid.

## Provenance

Every face is an unmodified upstream release, committed byte-for-byte. The
variable faces are shipped as published: the rasteriser instantiates a weight
from the `wght` axis at load time rather than a derived static file being
committed here.

| file                                       | SHA-256 (prefix) | upstream                                                                     |
|--------------------------------------------|------------------|------------------------------------------------------------------------------|
| `inter/Inter-Variable.ttf`                  | `29160a80ff49dd` | `github.com/google/fonts` `ofl/inter/Inter[opsz,wght].ttf`                    |
| `noto-sans/NotoSans-Variable.ttf`           | `bfb7bb691513f1` | `github.com/google/fonts` `ofl/notosans/NotoSans[wdth,wght].ttf`              |
| `noto-serif/NotoSerif-Variable.ttf`         | `4d8e6761424656` | `github.com/google/fonts` `ofl/notoserif/NotoSerif[wdth,wght].ttf`            |
| `sans-fallback/NotoSansHebrew-Variable.ttf` | `7ef36a2c359375` | `github.com/google/fonts` `ofl/notosanshebrew/NotoSansHebrew[wdth,wght].ttf`  |
| `sans-fallback/NotoSansSC-Variable.ttf`     | `a3041811a78c36` | `github.com/google/fonts` `ofl/notosanssc/NotoSansSC[wght].ttf`               |
| `sans-fallback/NotoSansKR-Variable.ttf`     | `194018e6b2b293` | `github.com/google/fonts` `ofl/notosanskr/NotoSansKR[wght].ttf`               |
| `mono/Inconsolata-EX.ttf`                   | `ef7a13bee56f6e` | Inconsolata EX                                                                |
| `mono/MPLUS1Code-Regular.ttf`               | `c5b8c7a2dc8fe8` | M PLUS 1 Code                                                                 |
| `mono/D2Coding-Regular.ttf`                 | `8b1b23e5de4dff` | D2Coding                                                                      |
| `mono/NotoSansHebrew-ExtraCondensed.ttf`    | `cb46b5153a5fb9` | Noto Sans Hebrew ExtraCondensed                                               |

Only bracket-free file names are used: a name like `Inter[opsz,wght].ttf`
would collide with the glob syntax every path consumer accepts.

Each family directory carries the SIL Open Font Licence text covering its
faces. Every face here is OFL-1.1 licensed and unmodified, so no reserved
font name is engaged.
