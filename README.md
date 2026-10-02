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
| `crates/fdmv-recorder` | `fdmv-recorder`（録画アプリ。画面を録画し、アプリごとの音声を別々のチェーンとして録音する） |
| `crates/fdmv-capture` | 録画の中核（画面・アプリ音声・マイクの取り込み、リアルタイムエンコード、FDMV への変換）。GUI に依存しない |
| `crates/fdmv-gui` | プレイヤーとエディタで共有する部品（音声出力、映像キュー、日本語フォント） |

## ビルド

共通して必要なもの:

- Rust 1.85 以上（[rustup](https://rustup.rs/)）
- dav1d と libopus（デコード用の C ライブラリ）と pkg-config
- `ffmpeg` / `ffprobe`（CLI の `pack` / `add-chain` / `export` などで外部コマンドとして呼び出す。
  AV1 エンコーダ `libsvtav1` / `libaom-av1` / `librav1e` のいずれかと `libopus` が必要。プレイヤーには不要）

| OS | 依存パッケージの入れ方 |
|---|---|
| Fedora | `sudo dnf install libdav1d-devel opus-devel alsa-lib-devel pipewire-devel clang pkgconf ffmpeg` |
| Debian / Ubuntu | `sudo apt install libdav1d-dev libopus-dev libasound2-dev libpipewire-0.3-dev libclang-dev pkg-config ffmpeg` |
| macOS | `brew install dav1d opus pkg-config ffmpeg` |
| Windows | MSYS2 の UCRT64 環境で `pacman -S mingw-w64-ucrt-x86_64-{rust,dav1d,opus,pkgconf,gcc,ffmpeg}` |

```sh
cargo build --release
# target/release/ に fdmv / fdmv-player / fdmv-editor / fdmv-recorder ができる
```

- Windows で MSYS2 を使う場合は UCRT64 のシェルでビルドする。実行時は `dav1d` / `libopus` の DLL（`C:\msys64\ucrt64\bin`）に PATH を通すか、exe と同じフォルダに置く。
- libopus をソースからビルドして静的リンクしたい場合は `--features libfdmv/bundled-opus`（cmake が必要）。
- Linux の PipeWire の開発用ファイル（`pipewire-devel` / `libpipewire-0.3-dev`）と clang は録画アプリの画面取り込みに使う（ビルド時のみ）。
- macOS での確認手順は [docs/macos-test.md](docs/macos-test.md)、Windows での録画アプリの確認手順は [docs/windows-test.md](docs/windows-test.md)。

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

## 録画アプリ

```sh
fdmv-recorder
```

画面（またはウィンドウ）を録画しながら、**選んだアプリの音声をアプリごとに別のチェーン**として録音する。

- **映像**: 左上で録画する画面・ウィンドウを選ぶ。Linux では「選ぶ…」で OS の画面共有ダイアログが開く。
- **音声**: 右の一覧で録音するアプリに ✓ を付け、そのうち 1 つを**メイン**にする。メインはデフォルトチェーン
  （普通に再生したときに鳴る音）、ほかはアプリ名のサブチェーンになる（名前は変えられる）。
  録画するウィンドウ以外のアプリの音も録れる。選んでいないアプリの音は入らない。
- **マイク**: 「マイクも録音する」で「マイク」というサブチェーンに録音する。
- 録画中に ✓ を付けたアプリは、その時点からチェーンとして追加される。
- 音が出ていない時間（2 秒以上の無音）はチェーンのセグメントに含めない（容量を使わない）。一度も音が出なかったチェーンは入れない。
- 保存先は既定で「ビデオ」フォルダ。ファイル名は `録画 日付 時刻.fdmv`。
- **エンコーダ**: 起動時に使える GPU を調べ、既定ではおすすめを自動で選ぶ。
  - GPU の AV1（RTX 40 以降の NVENC、Intel Arc、Radeon RX 7000 以降、VA-API）: 録画中も軽く、停止後すぐに保存される。
  - GPU の HEVC / H.264（それより前の GPU、macOS）: 録画中は軽い。FDMV は AV1 なので、停止後に CPU で AV1 へ変換する
    （1440p・60fps で録画時間と同じくらいかかる）。この ffmpeg で読み戻せる形式だけを使う（Fedora の ffmpeg-free には HEVC のデコーダが無いので H.264 になる）。
  - CPU の AV1（SVT-AV1）: GPU が使えないとき。高解像度・高フレームレートではとても重い。
- **圧縮**（AV1 で録るとき）: 「録画しながら圧縮」（既定。停止後すぐに保存）か「停止後に圧縮」（録画中は軽い設定で一時保存し、停止後に画質プリセットで圧縮し直す。小さくなるが保存に時間がかかる）。
- 映像は画面が変わったときだけ、録画開始からの時刻付きでエンコーダに渡す（可変フレームレート）。エンコードが追いつかないときはフレームを間引く（「コマ落ち」と表示）が、
  映像と音声はずれない。ffmpeg は優先度を下げて動かす（録画対象のゲームなどを妨げないように）。
- 録画中の一時ファイルは保存先の隠しフォルダ（`.録画 ….fdmvrec`）に置き、保存が終わると消す。

| OS | 画面 | アプリの音声 | マイク |
|---|---|---|---|
| Linux | xdg-desktop-portal の画面共有（Wayland / X11）＋ PipeWire | PulseAudio / PipeWire のアプリのストリーム（`pactl` / `parec` を使う） | `parec` |
| Windows | ffmpeg の gdigrab | WASAPI のプロセス単位ループバック（Windows 10 2004 以降） | cpal |
| macOS | ScreenCaptureKit | ScreenCaptureKit のアプリ単位の音声（macOS 13 以降） | cpal |

- Linux では、音声は**音を出しているアプリのストリーム**単位で選ぶ（ウィンドウ単位ではない。ブラウザのタブはまとめて 1 つのアプリになる）。
  一覧には音を出したことのあるアプリが出る。`pactl` / `parec`（Fedora では `pulseaudio-utils`）が必要。
  アプリがストリームを作り直しても追いかける（消えたストリームの録音は止め、PipeWire がマイクなどにつなぎ替えないようにしている）。
- macOS では初回に「画面収録」の許可が必要。
- 動作確認: Linux（KDE Wayland / PipeWire、RTX 3090）で確認済み。音と映像のずれは実測で 30 ms 未満（1440p・60fps・高負荷でも一定でずれていかない）。Windows / macOS は未確認（手順書あり）。
- GUI なしで試す: `cargo run --release -p fdmv-capture --example record -- --list`（`--encoders` で使えるエンコーダの一覧）

## テスト

```sh
cargo test --workspace   # ffmpeg を使うテストは、ffmpeg が無ければスキップされる
```

`.github/workflows/ci.yml` は Linux / macOS / Windows でビルドとテストを行う（GitHub に push すると動く）。
