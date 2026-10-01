# FDMV

AV1 の映像 1 本と、映像に同期して**重ねて**再生できる Opus 音声「チェーン」を複数本格納する動画フォーマット。

- **デフォルトチェーン**: 通常再生で鳴る音声
- **サブチェーン**: 名前付きの音声。デフォルトの上に好きなだけ重ねて鳴らせる（副音声・解説・効果音など）
- チェーンは動画の**一部区間だけ**に音声を持てる（区間外は無音で、容量も使わない）
- 完成したファイルに、映像を再エンコードせずに**チェーンを後から追加**できる

仕様は [SPEC.md](SPEC.md) を参照。

## 構成

| パス | 内容 |
|---|---|
| `crates/libfdmv` | ライブラリ。読み書き・追記・デコード（dav1d / libopus）・チェーンのミックス・ffmpeg による取り込み |
| `crates/fdmv-cli` | `fdmv` コマンド |
| `crates/fdmv-player` | `fdmv-player`（egui 製の GUI プレイヤー。Linux / macOS / Windows） |
| `crates/fdmv-editor` | `fdmv-editor`（簡易動画編集。タイムラインでカット・分割・範囲削除、チェーンへの音声配置） |
| `crates/fdmv-edit` | 編集の中核（プロジェクト、編集操作、プロキシ、ミックス、書き出し）。GUI に依存しない |
| `crates/fdmv-gui` | プレイヤーとエディタで共有する部品（音声出力、映像キュー、日本語フォント） |

## ビルド

共通して必要なもの:

- Rust 1.85 以上（[rustup](https://rustup.rs/)）
- dav1d と libopus（デコード用の C ライブラリ）と pkg-config
- `ffmpeg` / `ffprobe`（CLI の `pack` / `add-chain` / `export` などで外部コマンドとして呼び出す。
  AV1 エンコーダ `libsvtav1` / `libaom-av1` / `librav1e` のいずれかと `libopus` が必要。プレイヤーには不要）

| OS | 依存パッケージの入れ方 |
|---|---|
| Fedora | `sudo dnf install libdav1d-devel opus-devel alsa-lib-devel pkgconf ffmpeg` |
| Debian / Ubuntu | `sudo apt install libdav1d-dev libopus-dev libasound2-dev pkg-config ffmpeg` |
| macOS | `brew install dav1d opus pkg-config ffmpeg` |
| Windows | MSYS2 の UCRT64 環境で `pacman -S mingw-w64-ucrt-x86_64-{rust,dav1d,opus,pkgconf,gcc,ffmpeg}` |

```sh
cargo build --release
# target/release/ に fdmv / fdmv-player / fdmv-editor ができる
```

- Windows で MSYS2 を使う場合は UCRT64 のシェルでビルドする。実行時は `dav1d` / `libopus` の DLL（`C:\msys64\ucrt64\bin`）に PATH を通すか、exe と同じフォルダに置く。
- libopus をソースからビルドして静的リンクしたい場合は `--features libfdmv/bundled-opus`（cmake が必要）。
- macOS での確認手順は [docs/macos-test.md](docs/macos-test.md)。

## 使い方

```sh
# 動画の音声をデフォルトチェーンにし、解説を 2 か所に入れる
fdmv pack input.mp4 -o movie.fdmv \
  --chain "解説=part1.wav@0:12" --chain "解説=part2.wav@2:03.5" \
  --gain 解説=-3 --chain-meta 解説:language=ja --title "作品名"

# 後からサブチェーンを追加（元ファイルに追記）
fdmv add-chain movie.fdmv -n 効果音 -s se.wav@45 --channels 1 --in-place

fdmv info movie.fdmv                       # 情報（--json も可）
fdmv verify movie.fdmv                     # CRC・構造・デコードの検査
fdmv extract movie.fdmv -c 解説 -o kaisetsu.wav            # チェーンを取り出す（区間外は無音）
fdmv extract movie.fdmv -c main -c 解説 --apply-gain -o mix.flac   # ミックスして取り出す
fdmv export movie.fdmv -c 解説 -o share.mp4  # デフォルト＋解説を普通の動画に書き出す
fdmv extract-video movie.fdmv -o video.mkv   # 映像だけ（再エンコードなし）
fdmv snapshot movie.fdmv --at 1:23.4 -o frame.png
```

素材の指定は `PATH` または `PATH@開始時刻`（`12.5` / `1:02.5` / `01:02:03.25`）。
画質は `--quality`（`-Q`）で選ぶ。既定は `high`。`--crf` / `--preset` を指定するとそちらを優先し、`--ten-bit` で 10 bit 符号化になる。

| `--quality` | CRF / preset | VMAF（720p、細かい模様の多い映像） | 用途 |
|---|---|---|---|
| `best` | 18 / 6 | 約 97 | 元の映像と見分けがつかない画質 |
| `high`（既定） | 23 / 6 | 約 96 | 劣化がほとんど分からない |
| `standard` | 30 / 8 | 約 94 | サイズとのバランス |
| `small` | 38 / 8 | 90 前後 | 共有向けの小さいファイル |

SVT-AV1 では見た目重視のチューニング（`tune=0`）を使う。

## プレイヤー

```sh
fdmv-player movie.fdmv     # ファイルを指定して起動（ウィンドウへのドラッグ＆ドロップ、Ctrl/⌘+O でも開ける）
```

- 右の「チェーン」パネルでチェーンごとに ON/OFF と音量を切り替えられる（複数のサブチェーンを同時に重ねられる）。
  起動時はデフォルトチェーンだけが ON。
- シークバーの上の色付きの帯は、サブチェーンの音声がある区間。
- 操作: Space 再生/一時停止、←/→ 5 秒移動、↑/↓ 音量、M ミュート、F（またはダブルクリック）全画面、Esc 全画面解除。
- 日本語フォントは OS から自動で探す（Noto Sans CJK / ヒラギノ / 游ゴシック / メイリオ など）。
  見つからなければ英語表示になる。`FDMV_FONT=/path/to/font.ttf` で指定もできる。
- 環境変数: `FDMV_NO_AUDIO=1` で音声デバイスを使わない、`FDMV_DEBUG_SCREENSHOT=out.ppm` で
  自己テスト（全チェーンを ON にして 2 秒の位置から 1.5 秒再生し、画面を保存して終了）。

## エディタ

```sh
fdmv-editor                       # 新規
fdmv-editor project.fdmvproj      # プロジェクトを開く
fdmv-editor input.mp4             # 動画を読み込んで映像トラックに並べた状態で開始
```

- **素材**: 動画・音声ファイルをウィンドウにドロップ（または「＋ 追加」）。読み込むとプレビュー用のプロキシ
  （元の解像度・最大 1080p の AV1 と 48 kHz WAV）を OS のキャッシュディレクトリに作る。素材一覧からタイムラインへドラッグして配置する。
- **映像トラック**: 素材を並べた順に 1 本の動画になる（隙間なし）。ドラッグで並べ替え、端のドラッグでトリム。
- **チェーン**: 各行がチェーン 1 本。素材をドラッグするか「＋」で再生位置に音声を置く（重ねてもミックスされる）。
  デフォルトチェーンには映像クリップの音声が自動で含まれる（プロパティで切り替え可）。
- **カット**: S で再生位置で分割、Delete で選択クリップを削除（映像は後ろを詰める）、
  I / O でイン点・アウト点を決めて Shift+Delete で全トラックから範囲を削除して詰める。
- **元に戻す**: Ctrl+Z / Ctrl+Y。**保存**: Ctrl+S（`.fdmvproj`、JSON）。**書き出し**: Ctrl+E（ffmpeg で再エンコードして `.fdmv`）。
- 書き出しの解像度・フレームレートは既定で最初の映像クリップに合わせる（右パネルで変更可）。
  画質は右パネルのプリセット（最高画質／高画質／標準／小容量）で選ぶ。既定は高画質。
- GUI なしでの書き出し: `cargo run --release -p fdmv-edit --example export_project -- project.fdmvproj out.fdmv`

## テスト

```sh
cargo test --workspace   # ffmpeg を使うテストは、ffmpeg が無ければスキップされる
```

`.github/workflows/ci.yml` は Linux / macOS / Windows でビルドとテストを行う（GitHub に push すると動く）。
