[English](README.md) | **日本語**

# cpp-map

AIエージェント専用のコンテキスト圧縮CLI。C++Builderプロジェクトを対象に、
プロジェクト全体を読ませる代わりに「読むべきファイル・シンボル・依存関係」だけを
JSONで返す。人間向けレポートは生成しない（Markdown出力なし）。

Version: `1.0.0`  
Author: `Hiragi0w0`  
Project edition: `2026`  
Rust edition: `2024`

このツールは、古いC++Builder/VCLコードベースをAIエージェントで調査・修正するときの
「最初に全部読む」コストを下げるための補助ツールである。`.cpp` / `.h` / `.hpp` /
`.cc` / `.cxx` / `.cbproj` を走査し、ソース本文ではなく、パス、ロール、include関係、
シンボル候補、行番号、関連理由を小さなJSONとして返す。必要になった箇所だけ
`snippet` で本文を取り出す設計なので、巨大なフォーム実装やShift_JISの既存資産でも、
AIに渡すコンテキストを絞り込みやすい。

## 何に使うか

- C++Builderプロジェクトの入口、フォーム、リポジトリ、モデル、ユーティリティをざっくり把握する
- 日本語UI文言や機能名から、読むべきファイルをランキングする
- `.cpp` と `.h`、include先、include元、同名実装ファイルをたどる
- 関数・メソッド・クラス・`__property` の候補と行番号を取得する
- 変更前にシンボルの定義と参照箇所を分けて確認する
- MCPサーバーとして起動し、AIエージェントから同じ機能をツールとして呼び出す

## 基本方針

- 出力はAIエージェントが機械的に扱いやすいJSONを優先する
- ソース全文をインデックスに保存せず、必要な本文だけをライブファイルから切り出す
- 完全なC++ AST解析ではなく、C++Builderでよく出る構文に寄せたトレラントな候補抽出を行う
- 初回以降はmtime/sizeを見て、変更・追加・削除されたファイルだけを自動で再解析する
- `.dfm` やビルド成果物は対象外にし、コード調査に必要な最小限の構造情報に絞る

## ビルド

```bash
cargo build --release
# => target/release/cpp-map(.exe)
```

## インストール

GitHubから取得してローカルで使う場合は、Rust toolchainを入れたうえでビルドする。

```bash
git clone https://github.com/Hiragi0w0/cpp-map.git
cd cpp-map
cargo build --release
```

生成された実行ファイルは `target/release/cpp-map.exe`、Unix系環境では
`target/release/cpp-map` に置かれる。

## 使い方

```bash
# 最初に一度: インデックス生成 (.ai-context/index.json のみを生成)
cpp-map scan .

# プロジェクトの最小概要
cpp-map overview .

# ファイル一覧（ロール絞り込み・件数制限・NDJSON対応）
cpp-map files . --role form_or_dialog_logic --limit 20
cpp-map files . --ndjson

# 特定ファイルのシンボル候補（class / method / function / property + 行番号）
cpp-map symbols . --file MainForm.h

# include関係（プロジェクト内解決済み + 外部include + 逆依存）
cpp-map includes . --file MainForm.cpp

# 関連ファイル（同名ヘッダー/実装、include先、include元 を理由・スコア付きで）
cpp-map related . --file MainForm.cpp

# キーワードから関連ファイルをランキング（日本語キーワード可、Shift-JISソース対応）
cpp-map focus . "社員一覧"

# シンボル単位で本文だけ切り出す（ファイル丸読みの代替）
# --file 省略時はプロジェクト全体から検索し、宣言より定義本体を優先
cpp-map snippet . --symbol ButtonSaveClick
cpp-map snippet . --symbol LoadEmployees --file EmployeeListForm.cpp --context 2

# シンボルの参照箇所を逆引き（定義と呼び出し箇所を分離して返す）
cpp-map refs . LoadEmployees
```

`scan` が必要なのは初回だけ。以降の全クエリは実行時に mtime/size を比較し、
変更・追加・削除されたファイルだけを自動で再解析する（差分再スキャン）。

デフォルト出力はコンパクトなJSON。人間が読むときは `--pretty` を付ける。

## 典型的な調査フロー

初めて見るプロジェクトでは、まず `scan` で `.ai-context/index.json` を作り、
`overview` で入口候補と主要ディレクトリを確認する。

```bash
cpp-map scan C:\path\to\project
cpp-map overview C:\path\to\project --pretty
```

機能名、画面名、日本語UI文言などが分かっている場合は `focus` から始める。
返ってきた `suggested_reading_order` を読む順番の目安にし、候補ファイルごとに
`related`、`symbols`、`snippet` を組み合わせる。

```bash
cpp-map focus . "社員一覧" --pretty
cpp-map related . --file forms/EmployeeListForm.cpp --pretty
cpp-map symbols . --file forms/EmployeeListForm.h --pretty
cpp-map snippet . --symbol LoadEmployees --file forms/EmployeeListForm.cpp --context 2 --pretty
```

既存メソッドを変更する前には `refs` で定義と呼び出し箇所を分けて確認する。
`refs` はコメントと文字列を除いたコード上で単語境界マッチを行うため、
単純な全文検索よりも呼び出し箇所の確認に向いている。

```bash
cpp-map refs . LoadEmployees --pretty
```

## MCPサーバーとして使う

全コマンドをMCPツールとして公開するstdioサーバーを内蔵している。

```bash
# プロジェクトルートを固定して起動（各ツールで project_path 省略可）
cpp-map mcp C:\path\to\project

# ルートを固定せず起動（各ツール呼び出しで project_path 必須）
cpp-map mcp
```

Claude Code への登録例:

```bash
claude mcp add cpp-map -- C:\path\to\cpp-map.exe mcp C:\path\to\project
```

CLIと違い、インデックス未生成のままクエリを呼ぶと自動で `scan` してから応答する。
インデックス生成後の通常クエリでは、CLIと同じく変更・追加・削除されたファイルだけを
差分再スキャンする。強制的に作り直したい場合は `scan` ツールを呼ぶ。

MCPツール名はCLIサブコマンドと同じで、`scan`、`overview`、`files`、`symbols`、
`includes`、`related`、`focus`、`refs`、`snippet` を公開する。起動時に
プロジェクトルートを渡した場合、各ツールの `project_path` は省略できる。
ルートを固定せずに起動した場合は、ツール呼び出しごとに `project_path` が必要になる。

## 特徴

- `.cpp` `.h` `.hpp` `.cc` `.cxx` `.cbproj` のみ対象。`.dfm` やビルド成果物、
  `__history/` `Debug/` などのディレクトリは除外
- ソース本文は原則出力せず、パス・行番号・シンボル名・関連理由を返す
  （例外は `snippet`: 問い合わせたシンボルの範囲だけを `--max-lines` 上限付きで返す）
- インデックスと実ファイルの行ズレを検知すると `index_stale` エラーで再scanを促す
- 完全なAST解析はせず、コメント/文字列除去 + 正規表現によるトレラントな候補抽出
  （`__fastcall` `__published` `__property` を認識）
- ファイルはUTF-8 / UTF-16 BOM / Shift_JIS(cp932) を自動判別して読む
- 生成物は `.ai-context/index.json` のみ

## 技術的な構成

実装はRust製の単一バイナリで、CLIとMCPサーバーは同じコマンド実装を共有する。

- `src/main.rs`: CLI引数の定義とJSON出力
- `src/scan.rs`: プロジェクト走査、除外ディレクトリ判定、インデックス生成、差分再スキャン
- `src/parse.rs`: include、クラス、メソッド、関数、`__property` の候補抽出
- `src/index.rs`: `.ai-context/index.json` の保存形式、ファイル引数解決、同名ヘッダー/実装の対応付け
- `src/commands.rs`: `overview`、`files`、`focus`、`snippet` などのクエリ処理
- `src/mcp.rs`: JSON-RPC over stdio のMCP tools/list・tools/call実装

インデックスにはソース本文を保存しない。保存するのは、プロジェクト相対パス、ファイル種別、
推定ロール、解決済みinclude、外部include、シンボル候補、mtime/sizeなどのメタデータである。
`snippet` と `focus` の本文検索は、必要なときに実ファイルを読み直して行う。

include解決は、include元ディレクトリからの相対パス、プロジェクトルートからの相対パス、
同名basenameの順に候補を探す。シンボル抽出ではコメントと文字列を空白化して行番号を保ったまま、
C++Builderでよく使われる `__fastcall`、`__published`、`__property`、VCLフォーム基底クラス候補を扱う。

## 制限

- 完全なC++コンパイラやASTではないため、テンプレート、マクロ展開、条件コンパイルを厳密には解釈しない
- シンボルは「候補」であり、同名・オーバーロード・マクロ経由の定義は曖昧になる場合がある
- `.dfm` はフォーム判定のヒントとして直接解析せず、`.cpp` 内の `#pragma resource "*.dfm"` を見る
- `focus` はパス、ファイル名、シンボル名、include、本文出現回数を組み合わせたランキングであり、意味解析ではない
- `snippet` はインデックスの行番号を使うため、mtime/sizeで検出できない同一サイズ編集では `index_stale` を返すことがある

## ロール

`entry_point` `form_or_dialog_logic` `model` `repository` `utility`
`configuration` `resource` `unknown`

## テスト

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

GitHub Actions（Windows環境）では `cargo fmt --check` と
`cargo clippy --all-targets -- -D warnings` を実行する。clippy は全ターゲットを
コンパイルするため、ビルドの健全性も同時に検証される。
統合テスト（`tests/cli.rs` / `tests/mcp.rs`）は開発時のローカル実行用で、
公開リポジトリには含めていない。手元にテスト一式がある場合は `cargo test` で実行できる。

## ライセンス

MIT ライセンス（[`LICENSE-MIT`](LICENSE-MIT)）または Apache License 2.0
（[`LICENSE-APACHE`](LICENSE-APACHE)）のデュアルライセンスで公開する。
利用者はいずれかを選択できる。

明示的に別段の記載がない限り、あなたがこの成果物に含めることを意図して提出した
コントリビューションは、Apache-2.0 の定義に従い、上記のデュアルライセンスの下で
提供されるものとし、追加の条件は付されない。
