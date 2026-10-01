//! 日本語を表示できるシステムフォントを探して egui に登録する。
//!
//! egui の内蔵フォントには CJK の字形が無いため、OS ごとの代表的なフォントを順に探す。
//! 環境変数 `FDMV_FONT` にフォントファイルのパスを指定すると、それを優先する。

use std::sync::Arc;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

/// 優先順。最初に見つかったものを使う。
const FAMILIES: &[&str] = &[
    // Linux
    "Noto Sans CJK JP",
    "Noto Sans JP",
    "Source Han Sans JP",
    "IPAexGothic",
    "IPAGothic",
    "VL Gothic",
    "Takao Gothic",
    "Droid Sans Japanese",
    // macOS
    "Hiragino Sans",
    "Hiragino Kaku Gothic ProN",
    "Hiragino Kaku Gothic Pro",
    // Windows
    "Yu Gothic UI",
    "Yu Gothic",
    "Meiryo UI",
    "Meiryo",
    "MS UI Gothic",
    "MS Gothic",
    // その他（日本語以外の CJK フォントでも漢字・かなは概ね表示できる）
    "Noto Sans CJK SC",
    "Droid Sans Fallback",
    "Arial Unicode MS",
];

/// CJK フォントを登録する。見つかれば true。
pub fn install(ctx: &egui::Context) -> bool {
    let Some((name, data)) = find() else {
        return false;
    };
    let mut defs = FontDefinitions::default();
    defs.font_data.insert(name.clone(), Arc::new(data));
    // 欧文は既定フォントのまま、足りない字形を CJK フォントで補う。
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        defs.families.entry(family).or_default().push(name.clone());
    }
    ctx.set_fonts(defs);
    true
}

fn find() -> Option<(String, FontData)> {
    if let Some(path) = std::env::var_os("FDMV_FONT") {
        match std::fs::read(&path) {
            Ok(bytes) => return Some(("user-font".into(), FontData::from_owned(bytes))),
            Err(e) => eprintln!("FDMV_FONT: cannot read {}: {e}", path.to_string_lossy()),
        }
    }
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    for family in FAMILIES {
        let query = fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            weight: fontdb::Weight::NORMAL,
            ..Default::default()
        };
        let Some(id) = db.query(&query) else { continue };
        let found = db.with_face_data(id, |bytes, index| {
            let mut data = FontData::from_owned(bytes.to_vec());
            data.index = index;
            data
        });
        if let Some(data) = found {
            return Some((family.to_string(), data));
        }
    }
    None
}
