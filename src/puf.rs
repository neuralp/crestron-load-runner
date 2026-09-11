//! What the device reports after a PUF firmware update.
//!
//! `puf -results` answers with a component table introduced by a fixed line,
//! headed by dashed names joined with `+`, and then filled with pipe-delimited
//! rows. Nothing here decides what counts as a successful component: the値
//! device's own wording is carried through untouched.

use std::collections::BTreeMap;

const MARKER: &str = "The PUF update result by components:";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Results {
    pub headings: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Reads the component table out of a `puf -results` answer, or `None` when the
/// device did not report one.
pub fn parse(output: &str) -> Option<Results> {
    let mut lines = output
        .lines()
        .skip_while(|line| !line.contains(MARKER))
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.is_empty());
    // Column names come from the heading row itself. Nothing here expects a
    // particular set of them: they vary by device and by what was updated.
    let headings: Vec<String> = heading_cells(lines.next()?)
        .into_iter()
        .map(|heading| heading.trim_matches('-').trim().to_owned())
        .collect();
    if headings.len() < 2 || headings.iter().any(String::is_empty) {
        return None;
    }
    let mut rows = Vec::new();
    for line in lines {
        if is_rule(line) {
            continue;
        }
        // The table ends at the first line that is not a row of it. Stopping
        // beats reshaping: a short row would otherwise shift every cell.
        let Some(row) = row_cells(line, headings.len()) else {
            break;
        };
        rows.push(row.into_iter().map(str::to_owned).collect());
    }
    Some(Results { headings, rows })
}

impl Results {
    /// A one-line account for the device card: how many components the update
    /// touched, and how many landed on each result the device reported.
    pub fn summary(&self) -> String {
        let count = self.rows.len();
        let Some(column) = self.result_column() else {
            return format!("{count} component(s)");
        };
        let mut tally: BTreeMap<&str, usize> = BTreeMap::new();
        for row in &self.rows {
            *tally.entry(row[column].as_str()).or_default() += 1;
        }
        let detail = tally
            .iter()
            .map(|(result, count)| {
                let result = if result.is_empty() { "(blank)" } else { result };
                if *count == 1 {
                    result.to_owned()
                } else {
                    format!("{result} ×{count}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        if detail.is_empty() {
            format!("{count} component(s)")
        } else {
            format!("{count} component(s): {detail}")
        }
    }

    /// The whole table with its columns lined up, for the device log.
    pub fn as_text(&self) -> String {
        let widths: Vec<usize> = self
            .headings
            .iter()
            .enumerate()
            .map(|(column, heading)| {
                self.rows
                    .iter()
                    .filter_map(|row| row.get(column))
                    .map(|cell| cell.chars().count())
                    .chain(std::iter::once(heading.chars().count()))
                    .max()
                    .unwrap_or_default()
            })
            .collect();
        let line = |cells: &[String]| {
            cells
                .iter()
                .zip(&widths)
                .map(|(cell, width)| format!("{cell:width$}"))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_owned()
        };
        std::iter::once(line(&self.headings))
            .chain(self.rows.iter().map(|row| line(row)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The column the device named for the outcome. Without one, no column is
    /// guessed — a version would tally as though it were a result.
    fn result_column(&self) -> Option<usize> {
        self.headings.iter().position(|heading| {
            let heading = heading.to_ascii_lowercase();
            heading.contains("result") || heading.contains("status")
        })
    }
}

/// Names from the heading row. A heading is never blank, so a delimiter at
/// either end framed the row rather than opening a cell.
fn heading_cells(line: &str) -> Vec<&str> {
    let mut cells: Vec<&str> = line.trim().split('+').map(str::trim).collect();
    if cells.len() > 1 && cells.first() == Some(&"") {
        cells.remove(0);
    }
    if cells.len() > 1 && cells.last() == Some(&"") {
        cells.pop();
    }
    cells
}

/// Cells of one row, or `None` when the line is not a row of this table.
///
/// A blank cell at either end is ambiguous: a delimiter there either frames the
/// row or opens an empty cell, and tables come both ways. The reading that fits
/// the headings is the right one, which leaves an empty first or last column
/// intact in an unframed table.
fn row_cells(line: &str, columns: usize) -> Option<Vec<&str>> {
    if !line.contains('|') {
        return None;
    }
    let mut cells: Vec<&str> = line.trim().split('|').map(str::trim).collect();
    let blank = |cell: Option<&&str>| cell == Some(&"");
    if cells.len() == columns + 2 && blank(cells.first()) && blank(cells.last()) {
        cells.remove(0);
        cells.pop();
    } else if cells.len() == columns + 1 {
        if blank(cells.first()) {
            cells.remove(0);
        } else if blank(cells.last()) {
            cells.pop();
        }
    }
    (cells.len() == columns).then_some(cells)
}

fn is_rule(line: &str) -> bool {
    !line.is_empty()
        && line
            .chars()
            .all(|character| matches!(character, '-' | '+' | '|' | '=' | ' '))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = "\
puf -results
Loading results...

The PUF update result by components:
----Name-----------+---Version----+---Result---+
 Bootloader        | 1.0.2        | Success    |
 OS                | 2.8001.00049 | Success    |
 Application       | 2.8001.00049 | Failed     |
 Recovery          |              | Not needed |
----------------------------------------------+

Update finished.
";

    /// A real CP4N answer. The column names vary by device and by what was
    /// updated, so they are read from the heading row and never assumed.
    const CP4N: &str = "\
The PUF update result by components:
---Name----------------+---Slot---+-FW File Version----+-Device FW Version--+---Results---+-Signature-+---Description---
ROUTER_REV2            |          | v2.002.0041        | v2.002.0041        |    Pass     |    Pass   | Update Internal Router Rev2
ROUTER_BOOTLOADER_REV2 |          | v205               | v205               |    N/A      |    N/A    | Update Internal Router Bootloader Rev2
OS                     |          | v2.8005.00012      | v2.8005.00012      |    Pass     |    Pass   | Update CP4N OS
IOP                    |    6     | v1.3177.00007      | v1.3177.00007      |    N/A      |    N/A    | Update IO Processor
IOP_FPGA               |    6     | v10                | v10                |    N/A      |    N/A    | Update IO Processor FPGA
AVF                    |          | v6.3200.0005       | v6.3200.0005       |    Pass     |    Pass   | Update AVF Setup Program
-----------------------+----------+--------------------+--------------------+-------------+-----------+-----------------
";

    #[test]
    fn reads_a_real_processor_answer_whatever_its_columns_are_called() {
        let results = parse(CP4N).unwrap();
        assert_eq!(
            results.headings,
            [
                "Name",
                "Slot",
                "FW File Version",
                "Device FW Version",
                "Results",
                "Signature",
                "Description"
            ]
        );
        assert_eq!(results.rows.len(), 6);
        assert_eq!(
            results.rows[0],
            [
                "ROUTER_REV2",
                "",
                "v2.002.0041",
                "v2.002.0041",
                "Pass",
                "Pass",
                "Update Internal Router Rev2"
            ]
        );
        // An empty cell in the middle of a row is that column's value, not a
        // missing one, so the slot stays with the component that has it.
        assert_eq!(results.rows[3][1], "6");
        assert_eq!(results.rows[5][0], "AVF");
        // The rule closing the table is not a row, and does not end it early.
        assert_eq!(results.summary(), "6 component(s): N/A ×3, Pass ×3");
    }

    #[test]
    fn an_empty_cell_at_either_end_survives_an_unframed_row() {
        // The same shape as the processor's table, but with the blank column
        // first and last, where a framing delimiter would otherwise hide it.
        let results = parse(
            "The PUF update result by components:\n\
             ---Slot---+---Name---+---Results---\n\
             | OS       | Pass\n\
             6 | IOP      |\n",
        )
        .unwrap();
        assert_eq!(results.rows, [["", "OS", "Pass"], ["6", "IOP", ""]]);
        assert_eq!(results.summary(), "2 component(s): (blank), Pass");
    }

    #[test]
    fn a_framed_table_drops_the_frame_rather_than_reading_it_as_cells() {
        let results = parse(
            "The PUF update result by components:\n\
             +---Name---+---Results---+\n\
             | OS       | Pass        |\n\
             | IOP      |             |\n",
        )
        .unwrap();
        assert_eq!(results.headings, ["Name", "Results"]);
        assert_eq!(results.rows, [["OS", "Pass"], ["IOP", ""]]);
    }

    #[test]
    fn the_component_table_is_read_out_of_the_surrounding_chatter() {
        let results = parse(ANSWER).unwrap();
        assert_eq!(results.headings, ["Name", "Version", "Result"]);
        assert_eq!(results.rows.len(), 4);
        assert_eq!(results.rows[0], ["Bootloader", "1.0.2", "Success"]);
        // An empty cell between two delimiters is a real, empty cell.
        assert_eq!(results.rows[3], ["Recovery", "", "Not needed"]);
        assert_eq!(
            results.summary(),
            "4 component(s): Failed, Not needed, Success ×2"
        );
        // The log form lines the columns up, padding the blank version through.
        let text = results.as_text();
        let columns = |line: &str| {
            line.split("  ")
                .map(str::trim)
                .filter(|cell| !cell.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(columns(text.lines().next().unwrap()), results.headings);
        assert_eq!(columns(text.lines().nth(1).unwrap()), results.rows[0]);
        let recovery = text.lines().last().unwrap();
        assert!(recovery.starts_with("Recovery "), "{recovery:?}");
        assert!(recovery.ends_with("Not needed"), "{recovery:?}");
    }

    #[test]
    fn a_table_without_the_frame_or_the_result_column_still_reads() {
        let results = parse(
            "The PUF update result by components:\n\
             ----Name----+----Version----\n\
             Bootloader  | 1.0.2\n\
             OS          | 2.8001.00049\n",
        )
        .unwrap();
        assert_eq!(results.headings, ["Name", "Version"]);
        assert_eq!(results.rows.len(), 2);
        // No column is named for the outcome, so none is invented.
        assert_eq!(results.summary(), "2 component(s)");
    }

    #[test]
    fn the_table_stops_at_the_first_line_that_is_not_one_of_its_rows() {
        let results = parse(
            "The PUF update result by components:\n\
             ----Name----+----Result----\n\
             Bootloader  | Success\n\
             OS          | Success | extra\n\
             Application | Success\n",
        )
        .unwrap();
        assert_eq!(results.rows.len(), 1, "a short row must not shift cells");
        assert_eq!(results.summary(), "1 component(s): Success");
    }

    #[test]
    fn answers_without_a_component_table_report_nothing() {
        for output in [
            "",
            "puf -results\nNo update has been performed.\n",
            // The marker with nothing usable behind it.
            "The PUF update result by components:\n",
            "The PUF update result by components:\nnot a heading row\n",
            "The PUF update result by components:\n----Name----\n Bootloader\n",
            "The PUF update result by components:\n----+----\n a | b\n",
        ] {
            assert!(parse(output).is_none(), "{output:?}");
        }
    }

    #[test]
    fn a_marked_table_with_no_rows_is_still_a_result() {
        let results = parse("The PUF update result by components:\n--Name--+--Result--\n").unwrap();
        assert!(results.rows.is_empty());
        assert_eq!(results.summary(), "0 component(s)");
    }
}
