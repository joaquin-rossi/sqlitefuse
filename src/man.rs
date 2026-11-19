//! The man page: what clap knows about the arguments, plus help.txt
//! converted to roff.

use clap_mangen::Man;
use std::io::{self, Write};

pub fn render(cmd: clap::Command, w: &mut dyn Write) -> io::Result<()> {
    let man = Man::new(cmd.clone());
    man.render_title(w)?;
    man.render_name_section(w)?;
    man.render_synopsis_section(w)?;
    writeln!(w, ".SH DESCRIPTION")?;
    writeln!(
        w,
        "Mounts an SQLite database as a read-only directory where every table and view is a \
         JSONL file, with one JSON object per row."
    )?;
    man.render_options_section(w)?;
    if let Some(help) = cmd.get_after_long_help() {
        w.write_all(help_to_roff(&help.to_string()).as_bytes())?;
    }
    writeln!(
        w,
        ".SH \"SEE ALSO\"\n.BR sqlite3 (1),\n.BR fusermount3 (1),\n.BR mount.fuse3 (8)"
    )?;
    man.render_version_section(w)?;
    man.render_authors_section(w)?;
    Ok(())
}

const CODE_INDENT: usize = 6;

/// The conventions of help.txt: `HEADING:` lines are sections, `- ` starts an
/// item, lines indented by CODE_INDENT are code, and blank lines separate
/// paragraphs.
fn help_to_roff(text: &str) -> String {
    let indent = |line: &str| line.len() - line.trim_start().len();
    let is_code = |line: &str| !line.trim().is_empty() && indent(line) >= CODE_INDENT;
    let lines: Vec<&str> = text.lines().collect();

    let mut out = String::new();
    let mut in_code = false;
    let mut new_paragraph = false;
    for (i, &line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            // A blank line between code lines is part of the code.
            let more_code = lines[i + 1..]
                .iter()
                .find(|l| !l.trim().is_empty())
                .is_some_and(|l| is_code(l));
            if in_code && more_code {
                out.push('\n');
            } else {
                new_paragraph = true;
            }
            continue;
        }

        if is_code(line) {
            if !in_code {
                out.push_str(".PP\n.RS\n.nf\n");
                in_code = true;
            }
            out.push_str(&escape(&line[CODE_INDENT..]));
            out.push('\n');
            continue;
        }
        if in_code {
            out.push_str(".fi\n.RE\n");
            in_code = false;
        }

        let text = line.trim();
        if indent(line) == 0 && text.ends_with(':') {
            out.push_str(&format!(".SH \"{}\"\n", text.trim_end_matches(':')));
        } else if let Some(item) = text.strip_prefix("- ") {
            out.push_str(".IP \\(bu 2\n");
            out.push_str(&escape(item));
            out.push('\n');
        } else {
            if new_paragraph {
                out.push_str(".PP\n");
            }
            out.push_str(&escape(text));
            out.push('\n');
        }
        new_paragraph = false;
    }
    if in_code {
        out.push_str(".fi\n.RE\n");
    }
    out
}

fn escape(text: &str) -> String {
    let text = text.replace('\\', "\\e").replace('-', "\\-");
    // A leading . or ' would be read as a request.
    if text.starts_with(['.', '\'']) {
        format!("\\&{text}")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_conventions() {
        let help = "\
HEADING:
  Some text
  that goes on.

      code -x
      .hidden

      more code

  - an item
    continued
  - another

  After the list.
";
        assert_eq!(
            help_to_roff(help),
            "\
.SH \"HEADING\"
Some text
that goes on.
.PP
.RS
.nf
code \\-x
\\&.hidden

more code
.fi
.RE
.IP \\(bu 2
an item
continued
.IP \\(bu 2
another
.PP
After the list.
"
        );
    }

    #[test]
    fn renders() {
        use clap::CommandFactory;
        let mut out = Vec::new();
        render(crate::Cli::command(), &mut out).unwrap();
        let page = String::from_utf8(out).unwrap();
        for section in [
            ".SH NAME",
            ".SH SYNOPSIS",
            ".SH OPTIONS",
            ".SH \"MOUNT OPTIONS\"",
            ".SH \"SEE ALSO\"",
        ] {
            assert!(page.contains(section), "missing {section}");
        }
    }
}
