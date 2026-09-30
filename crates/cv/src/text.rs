//! Getting text out of an uploaded CV. Validates the *content*, not just the name: a PDF
//! must start with the PDF magic bytes, and a text file must be UTF-8 without binary junk.

use std::path::Path;

/// Larger uploads are refused.
pub const MAX_BYTES: usize = 5 * 1024 * 1024;
/// Text shorter than this isn't a CV (or a scanned PDF with no text layer).
const MIN_CHARS: usize = 40;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CvError {
    #[error("the file is empty")]
    Empty,
    #[error("the file is larger than {} MB", MAX_BYTES / 1024 / 1024)]
    TooLarge,
    #[error("unsupported file type: send a PDF, or a .md / .markdown / .txt file")]
    UnsupportedType,
    #[error("the file is named .pdf but is not a PDF")]
    NotAPdf,
    #[error("the file is a PDF; give it a .pdf name")]
    PdfWithOtherName,
    #[error("the text file is not valid UTF-8 text")]
    NotText,
    #[error("the PDF could not be read")]
    UnreadablePdf,
    #[error(
        "no extractable text; please upload a text-based PDF or a .md version (scanned images are not supported)"
    )]
    NoText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pdf,
    Text,
}

fn kind_by_name(filename: &str) -> Option<Kind> {
    let ext = Path::new(filename)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    return match ext.as_str() {
        "pdf" => Some(Kind::Pdf),
        "md" | "markdown" | "txt" => Some(Kind::Text),
        _ => None,
    };
}

/// The CV's plain text, whitespace-tidied but with its line structure kept (the parser
/// reads headings and titles line by line).
pub fn extract(bytes: &[u8], filename: &str) -> Result<String, CvError> {
    if bytes.is_empty() {
        return Err(CvError::Empty);
    }
    if bytes.len() > MAX_BYTES {
        return Err(CvError::TooLarge);
    }
    let named = kind_by_name(filename).ok_or(CvError::UnsupportedType)?;
    let is_pdf = bytes.starts_with(b"%PDF-");
    let raw = match (named, is_pdf) {
        (Kind::Pdf, false) => return Err(CvError::NotAPdf),
        (Kind::Text, true) => return Err(CvError::PdfWithOtherName),
        (Kind::Pdf, true) => pdf_text(bytes)?,
        (Kind::Text, false) => {
            let text = std::str::from_utf8(bytes).map_err(|_| CvError::NotText)?;
            let text = text.strip_prefix('\u{feff}').unwrap_or(text);
            if text.contains('\0') {
                return Err(CvError::NotText);
            }
            text.to_string()
        }
    };
    let text = tidy(&raw);
    if text.chars().filter(|c| !c.is_whitespace()).count() < MIN_CHARS {
        return Err(CvError::NoText);
    }
    return Ok(text);
}

/// PDF text extraction. The parser has been known to panic on odd files, so it runs
/// behind `catch_unwind`.
fn pdf_text(bytes: &[u8]) -> Result<String, CvError> {
    let outcome = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes));
    return match outcome {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(_)) | Err(_) => Err(CvError::UnreadablePdf),
    };
}

/// Trims lines, drops control characters and collapses runs of blank lines.
fn tidy(raw: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in raw.replace("\r\n", "\n").replace('\r', "\n").lines() {
        let line: String = line
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .collect();
        let line = line.trim();
        if line.is_empty() {
            blank += 1;
            if blank == 1 && !out.is_empty() {
                out.push('\n');
            }
            continue;
        }
        blank = 0;
        out.push_str(line);
        out.push('\n');
    }
    return out.trim_end().to_string();
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal single-page PDF whose page shows `lines` in Helvetica. Offsets are
    /// computed, so the cross-reference table is valid.
    pub(crate) fn pdf_with_text(lines: &[&str]) -> Vec<u8> {
        let mut content = String::from("BT /F1 12 Tf 14 TL 50 750 Td\n");
        for line in lines {
            let escaped = line
                .replace('\\', "\\\\")
                .replace('(', "\\(")
                .replace(')', "\\)");
            content.push_str(&format!("({escaped}) Tj T*\n"));
        }
        content.push_str("ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_string(),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend(format!("{} 0 obj\n{body}\nendobj\n", i + 1).into_bytes());
        }
        let xref = pdf.len();
        pdf.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).into_bytes());
        for offset in offsets {
            pdf.extend(format!("{offset:010} 00000 n \n").into_bytes());
        }
        pdf.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .into_bytes(),
        );
        return pdf;
    }

    const CV: &str =
        "Jane Doe\nBackend Engineer with eight years of experience building APIs in Rust.";

    #[test]
    fn reads_markdown_and_text() {
        let text = extract(
            format!("\u{feff}{CV}\r\n\r\n\r\n\r\nSkills\r\nRust").as_bytes(),
            "cv.MD",
        )
        .unwrap();
        assert!(text.starts_with("Jane Doe\nBackend Engineer"));
        assert!(
            text.contains("\n\nSkills\nRust"),
            "blank runs collapse to one: {text:?}"
        );
        assert!(extract(CV.as_bytes(), "cv.txt").is_ok());
        assert!(extract(CV.as_bytes(), "cv.markdown").is_ok());
    }

    #[test]
    fn reads_a_text_pdf() {
        let pdf = pdf_with_text(&[
            "Jane Doe",
            "Backend Engineer with eight years of experience",
            "Skills: Rust, SQL",
        ]);
        let text = extract(&pdf, "cv.pdf").unwrap();
        assert!(
            text.contains("Jane Doe") && text.contains("Rust, SQL"),
            "{text:?}"
        );
    }

    #[test]
    fn content_must_match_the_name() {
        let pdf = pdf_with_text(&["Jane Doe Backend Engineer with plenty of experience in Rust"]);
        assert_eq!(extract(&pdf, "cv.txt"), Err(CvError::PdfWithOtherName));
        assert_eq!(extract(CV.as_bytes(), "cv.pdf"), Err(CvError::NotAPdf));
        assert_eq!(
            extract(CV.as_bytes(), "cv.docx"),
            Err(CvError::UnsupportedType)
        );
        assert_eq!(extract(CV.as_bytes(), "cv"), Err(CvError::UnsupportedType));
        assert_eq!(
            extract(&[0xff, 0xfe, 0x00, 0x41], "cv.txt"),
            Err(CvError::NotText)
        );
        assert_eq!(extract(b"text\0with\0nul", "cv.txt"), Err(CvError::NotText));
    }

    #[test]
    fn rejects_empty_huge_and_textless_files() {
        assert_eq!(extract(b"", "cv.txt"), Err(CvError::Empty));
        assert_eq!(
            extract(&vec![b'a'; MAX_BYTES + 1], "cv.txt"),
            Err(CvError::TooLarge)
        );
        assert_eq!(extract(b"hi", "cv.txt"), Err(CvError::NoText));
        // A PDF with a page but no text, like a scan.
        assert_eq!(extract(&pdf_with_text(&[]), "cv.pdf"), Err(CvError::NoText));
        assert_eq!(
            extract(b"%PDF-1.4\ngarbage", "cv.pdf"),
            Err(CvError::UnreadablePdf)
        );
    }
}
