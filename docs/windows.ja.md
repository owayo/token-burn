# Windows での実行

Windows 版は x64 MSVC 向けです。ワーカーの端末に ConPTY、タスクの実行に Git for Windows の Bash を使います。動作には Windows 10 バージョン 1809 以降、または Windows 11 が必要です。導入方法は [README](../README.ja.md#インストール) を参照してください。

## 実行前の準備

Git for Windows と、利用するエージェントの CLI を用意します。エージェントへログインし、対象リポジトリで実行できることを確認してください。token-burn は Windows Terminal や PowerShell から実行できます。tmux の導入は不要です。

Git Bash は `TOKEN_BURN_BASH`、`CLAUDE_CODE_GIT_BASH_PATH`、`PATH` 上のディレクトリ、通常のシステム向けインストール先、ユーザー向けインストール先の順に探します。使用するのは Git に付属する `usr/bin/bash.exe` です。`System32` にある WSL の `bash.exe` は使いません。

Git を別の場所へ入れた場合は、実行前に `TOKEN_BURN_BASH` へ `bin/bash.exe` または `usr/bin/bash.exe` のパスを指定してください。

```powershell
$env:TOKEN_BURN_BASH = 'D:\Tools\Git\bin\bash.exe'
token-burn status
token-burn run -n
```

runner はこの設定を引き継ぐため、デタッチ後も有効です。タスク実行時の Bash は対話シェルのエイリアスを読みません。エージェントのコマンドには、PowerShell の関数やエイリアスではなく、実行ファイルやスクリプトを指定してください。

エージェントにはネイティブの実行ファイルか、`.cmd`・`.bat` の拡張子がない Bash/npm の shim を使ってください。これらの拡張子を持つバッチファイルは `cmd.exe` を経由し、プロンプト内の文字を解釈し直す場合があります。また、Windows のプロセスのコマンドライン全体には、終端を含めて 32,767 UTF-16 単位の上限があります。引数で渡すプロンプトも、この範囲に収める必要があります。

ワーカーでは、Git Bash がプロンプトの引数を書き換えないように `MSYS2_ARG_CONV_EXCL=*` を設定します。この設定はエージェントにも引き継がれます。エージェントが Bash から Windows の実行ファイルを呼ぶ場合は、`C:/Work/file.py` のような Windows のパスを使ってください。`/tmp/file.py` のような MSYS のパスは自動変換されません。

## パスと設定

TOML 内のパスには `/` を使うか、バックスラッシュをそのまま書ける一重引用符を使います。

```toml
[[scan]]
base_dirs = ['C:\Work\GitHub', 'D:\Projects']
```

対象・レポート・実行ファイルのパスには、空白や日本語を使えます。PowerShell では空白を含むパスを引用符で囲んでください。実行中の TUI で `a` を押すと、対象ディレクトリを追加できます。相対パスは、接続したクライアントの作業ディレクトリを基準に解決します。

Windows でも、設定したコマンドとアカウントの profile を使って ai-usage と連携します。利用環境で動く ai-usage を用意してください。利用できない場合は、[README](../README.ja.md#設定) の固定スケジュールを使います。

## デタッチと停止

`d` または `q` でデタッチし、`token-burn attach` で再接続できます。クライアントの端末を閉じても、独立した runner が実行を続けます。通常操作で Ctrl-C を押すと穏当な停止、もう一度押すと強制停止です。強制停止では Windows の Job Object を使い、ワーカーから起動した子孫プロセスも終了させます。

runner は起動時に親の Job Object から抜けることを試みます。ホストが許可しなければ、警告を出して同じ Job 内で起動します。その場合はホストの Job が閉じられると停止し得るため、ホスト終了後も実行を続けるには、その制約のない端末から起動してください。

Windows からサインアウトすると、runner を含むユーザーのプロセスは終了します。また、ワーカー終了時は、その Job Object に残った子孫プロセスも終了させます。

モニターには Windows の CPU 使用率・物理メモリ・ディスク容量を表示します。Unix のロードアベレージ、圧縮メモリ、スワップの計測値は Windows では取得せず、取得不可として表示します。

## ソースからのビルド

README の Cargo による導入には、Rust と MSVC 向けの Visual Studio C++ ビルドツールが必要です。Makefile を使う場合は、GNU Make と Bash の実行環境も用意してください。CI の Windows ジョブには、MSVC バイナリのビルド、clippy、単体テスト、実際の runner を起動するテストを組み込んでいます。runner のテストでは日本語を含むパス、対象追加、再接続、子孫プロセスの強制停止を確認します。
