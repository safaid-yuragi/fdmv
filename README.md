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

## ビルド

共通して必要なもの:

- Rust 1.85 以上（[rustup](https://rustup.rs/)）
- dav1d と libopus（デコード用の C ライブラリ）と pkg-config
- `ffmpeg` / `ffprobe`（CLI の `pack` / `add-chain` / `export` などで外部コマンドとして呼び出す。
  AV1 エンコーダ `libsvtav1` / `libaom-av1` / `librav1e` のいずれかと `libopus` が必要。プレイヤーには不要）

| OS | 依存パッケージの入れ方 |
|---|---|
| Fedora | `sudo dnf install dav1d-devel opus-devel alsa-lib-devel pkgconf ffmpeg` |
| Debian / Ubuntu | `sudo apt install libdav1d-dev libopus-dev libasound2-dev pkg-config ffmpeg` |
| macOS | `brew install dav1d opus pkg-config ffmpeg` |
| Windows | MSYS2 の UCRT64 環境で `pacman -S mingw-w64-ucrt-x86_64-{rust,dav1d,opus,pkgconf,gcc,ffmpeg}` |

```sh
cargo build --release
# target/release/fdmv と target/release/fdmv-player ができる
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
画質は `--crf`（0–63、既定 32）と `--preset`（SVT-AV1 は 0–13、既定 8）で調整する。`--ten-bit` で 10 bit 符号化。

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

## テスト

```sh
cargo test --workspace   # ffmpeg を使うテストは、ffmpeg が無ければスキップされる
```

`.github/workflows/ci.yml` は Linux / macOS / Windows でビルドとテストを行う（GitHub に push すると動く）。
