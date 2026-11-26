//! Turns user files into Messages content blocks.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

const MAX_IMAGE: u64 = 3_700_000; // 5 MB once base64 encoded
const MAX_PDF: u64 = 20_000_000;
const MAX_PDF_PAGES: u32 = 100;
const MAX_CHARS: usize = 400_000;
const MAX_FILE: u64 = 100_000_000;

#[derive(Clone, Debug)]
pub struct Attachment {
    pub name: String,
    pub blocks: Vec<Value>,
    /// Approximate size of the encoded content.
    pub bytes: usize,
}

fn text_attachment(name: &str, text: String) -> Result<Attachment> {
    if text.trim().is_empty() {
        bail!("{name} contains no readable text");
    }
    let count = text.chars().count();
    if count > MAX_CHARS {
        bail!("{name} is too long ({count} characters, the limit is {MAX_CHARS}). Split it into parts");
    }
    let bytes = text.len();
    let body = format!("<file name=\"{name}\">\n{text}\n</file>");
    Ok(Attachment {
        name: name.into(),
        blocks: vec![json!({"type": "text", "text": body})],
        bytes,
    })
}

fn binary_attachment(name: &str, kind: &str, media: &str, data: &[u8]) -> Attachment {
    let encoded = STANDARD.encode(data);
    let bytes = encoded.len();
    Attachment {
        name: name.into(),
        blocks: vec![
            json!({"type": "text", "text": format!("Attached file: {name}")}),
            json!({"type": kind, "source": {"type": "base64", "media_type": media, "data": encoded}}),
        ],
        bytes,
    }
}

pub fn image_attachment(name: &str, media: &str, data: &[u8]) -> Attachment {
    binary_attachment(name, "image", media, data)
}

fn image_type(data: &[u8]) -> Option<&'static str> {
    match data {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

/// Extracts visible text from document XML.
pub fn xml_text(xml: &str) -> String {
    let mut out = String::new();
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(len) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = &rest[start + 1..start + len];
        // Paragraph, row and line-break boundaries become newlines.
        let name = tag.trim_start_matches('/').split([' ', '/']).next().unwrap_or("");
        let closing = tag.starts_with('/') || tag.ends_with('/');
        if closing
            && matches!(
                name,
                "w:p" | "a:p" | "text:p" | "text:h" | "row" | "table:table-row" | "w:br"
            )
        {
            out.push('\n');
        } else if closing && matches!(name, "c" | "table:table-cell" | "w:tab") {
            out.push('\t');
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn natural_key(name: &str) -> (usize, String) {
    let digits: String = name.chars().filter(char::is_ascii_digit).collect();
    (digits.parse().unwrap_or(0), name.to_string())
}

fn read_entry<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<String> {
    let mut s = String::new();
    // Bounded, so a crafted archive cannot exhaust memory.
    zip.by_name(name)?.take(30_000_000).read_to_string(&mut s)?;
    Ok(s)
}

/// Spreadsheet cells refer to a shared string table; resolve those references.
pub fn sheet_text(sheet: &str, shared: &[String]) -> String {
    let mut out = String::new();
    for row in sheet.split("</row>") {
        let mut cells = Vec::new();
        for cell in row.split("<c ").skip(1) {
            let attrs = cell.split('>').next().unwrap_or("");
            let body = cell.split("</c>").next().unwrap_or("");
            let value = if attrs.ends_with('/') {
                String::new()
            } else if let Some(v) = body.split("<v>").nth(1).and_then(|v| v.split("</v>").next()) {
                if attrs.contains("t=\"s\"") {
                    v.parse::<usize>()
                        .ok()
                        .and_then(|i| shared.get(i).cloned())
                        .unwrap_or_default()
                } else {
                    xml_text(v)
                }
            } else {
                xml_text(body.split_once('>').map_or("", |b| b.1))
            };
            cells.push(value);
        }
        if cells.iter().any(|c| !c.is_empty()) {
            out.push_str(&cells.join("\t"));
            out.push('\n');
        }
    }
    out
}

fn office_text(path: &Path, ext: &str) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(file).context("not a valid document")?;
    let mut names: Vec<String> = zip.file_names().filter_map(Result::ok).map(String::from).collect();
    names.sort_by_key(|n| natural_key(n));
    let mut out = String::new();
    if ext == "xlsx" {
        let shared: Vec<String> = read_entry(&mut zip, "xl/sharedStrings.xml")
            .unwrap_or_default()
            .split("</si>")
            .filter_map(|si| {
                si.split_once("<si")
                    .map(|s| xml_text(&format!("<si{}", s.1)).trim().to_string())
            })
            .collect();
        for n in names
            .iter()
            .filter(|n| n.starts_with("xl/worksheets/") && n.ends_with(".xml"))
        {
            let sheet = read_entry(&mut zip, n)?;
            out.push_str(&format!("--- {n} ---\n{}", sheet_text(&sheet, &shared)));
        }
    } else {
        let wanted = |n: &str| match ext {
            "docx" => n == "word/document.xml" || n == "word/footnotes.xml",
            "pptx" => {
                (n.starts_with("ppt/slides/slide") || n.starts_with("ppt/notesSlides/notes")) && n.ends_with(".xml")
            }
            _ => n == "content.xml",
        };
        for n in names.iter().filter(|n| wanted(n)) {
            let xml = read_entry(&mut zip, n)?;
            if ext == "pptx" {
                out.push_str(&format!("\n--- {n} ---\n"));
            }
            out.push_str(&xml_text(&xml));
        }
    }
    Ok(out)
}

fn pdf_pages(path: &Path) -> Option<u32> {
    let out = pdf_tool("pdfinfo").arg(std::path::absolute(path).ok()?).env("LC_ALL", "C").output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("Pages:"))
        .and_then(|n| n.trim().parse().ok())
}

fn pdf_tool(name: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let executable = std::env::current_exe().ok();
        let bundled = executable.as_deref().and_then(Path::parent).and_then(Path::parent)
            .map(|root| root.join("poppler").join("bin").join(format!("{name}.exe")))
            .filter(|path| path.is_file());
        let mut command = Command::new(bundled.unwrap_or_else(|| format!("{name}.exe").into()));
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        command
    }
    #[cfg(not(windows))]
    Command::new(name)
}

fn pdf_text_command() -> Command {
    let mut command = pdf_tool("pdftotext");
    command.args(["-layout", "-enc", "UTF-8"]);
    command
}

fn extracted_pdf_text(output: Output) -> Result<String> {
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Converts a pending PDF to text for providers that only take text and images.
pub fn pdf_text_for_model(encoded: &str) -> Result<String> {
    let data = STANDARD.decode(encoded).context("invalid PDF attachment")?;
    let mut child = pdf_text_command().args(["-", "-"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().context("cannot run pdftotext (install Poppler)")?;
    let mut stdin = child.stdin.take().context("cannot open pdftotext input")?;
    let writer = std::thread::spawn(move || stdin.write_all(&data));
    let output = child.wait_with_output();
    let written = writer.join().map_err(|_| anyhow::anyhow!("cannot write to pdftotext"));
    let text = extracted_pdf_text(output?)?;
    written??;
    if text.trim().is_empty() {
        bail!("the PDF contains no readable text");
    }
    if text.chars().count() > MAX_CHARS {
        bail!("the PDF exceeds the {MAX_CHARS}-character limit. Split it into parts");
    }
    Ok(text)
}

pub fn load(path: &Path) -> Result<Attachment> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().replace(['"', '<', '>', '\n'], "_"))
        .context("not a file")?;
    let meta = std::fs::metadata(path).with_context(|| format!("cannot open {name}"))?;
    if !meta.is_file() {
        bail!("{name} is not a regular file (attach files, not folders)");
    }
    if meta.len() > MAX_FILE {
        bail!("{name} is larger than 100 MB");
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    if matches!(ext.as_str(), "docx" | "pptx" | "xlsx" | "odt" | "odp" | "ods") {
        return text_attachment(
            &name,
            office_text(path, &ext).with_context(|| format!("cannot read {name}"))?,
        );
    }

    let data = std::fs::read(path).with_context(|| format!("cannot read {name}"))?;
    if let Some(media) = image_type(&data) {
        if meta.len() > MAX_IMAGE {
            bail!("{name} is too large for an image (limit 3.7 MB). Resize it first");
        }
        return Ok(binary_attachment(&name, "image", media, &data));
    }
    if data.starts_with(b"%PDF") {
        // Small PDFs are sent whole, so figures and tables stay visible.
        let pages = pdf_pages(path);
        if meta.len() <= MAX_PDF && pages.is_some_and(|p| p <= MAX_PDF_PAGES) {
            return Ok(binary_attachment(&name, "document", "application/pdf", &data));
        }
        let out = pdf_text_command()
            .arg(std::path::absolute(path)?)
            .arg("-")
            .output()
            .context("cannot run pdftotext (install poppler)")?;
        let text = extracted_pdf_text(out).with_context(|| format!("cannot extract text from {name}"))?;
        return text_attachment(&name, text);
    }
    match String::from_utf8(data) {
        Ok(text) if !text.contains('\0') => text_attachment(&name, text),
        _ => bail!("{name}: unsupported binary format. Use text, code, PDF, images or Office documents"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, data: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("clueless-test-{}-{name}", std::process::id()));
        std::fs::write(&p, data).unwrap();
        p
    }

    #[test]
    fn loads_text_and_images_and_rejects_binary() {
        let t = temp("a.md", "# Привіт".as_bytes());
        let a = load(&t).unwrap();
        assert!(a.blocks[0]["text"].as_str().unwrap().contains("Привіт"));
        let i = temp("a.png", &[0x89, b'P', b'N', b'G', 1, 2, 3]);
        assert_eq!(load(&i).unwrap().blocks[1]["source"]["media_type"], "image/png");
        let b = temp("a.bin", &[0, 159, 146, 150]);
        assert!(load(&b).is_err());
        for p in [t, i, b] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn reads_docx() {
        use std::io::Write;
        let p = temp("a.docx", b"");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
        w.start_file("word/document.xml", zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(b"<w:body><w:p><w:r><w:t>A &amp; B</w:t></w:r></w:p><w:p><w:t>C</w:t></w:p></w:body>")
            .unwrap();
        w.finish().unwrap();
        let a = load(&p).unwrap();
        assert!(a.blocks[0]["text"].as_str().unwrap().contains("A & B\nC\n"));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn resolves_shared_strings() {
        let sheet = r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1"><v>42</v></c></row>"#;
        assert_eq!(sheet_text(sheet, &["name".into()]), "name\t42\n");
    }
}
