use tinymist_std::path::PathClean;

use crate::artifact::{cli, GIT_ROOT};

macro_rules! apply_common_filters {
    {} => {
        let mut settings = insta::Settings::clone_current();
        // Env redaction
        // [env: key=value]
        settings.add_filter(r"\[env:\s*([^=]+)=[^\]]*\]", "[env: $1=REDACTED]");
        settings.add_filter(r"tinymist.exe", "tinymist");
        let _bound = settings.bind_to_scope();
    }
}

#[test]
fn test_probe() {
    insta_cmd::assert_cmd_snapshot!(cli().arg("probe"), @r"
    success: true
    exit_code: 0
    ----- stdout -----
    
    ----- stderr -----
    ");
}

#[test]
fn test_help() {
    apply_common_filters!();
    insta_cmd::assert_cmd_snapshot!(cli().arg("--help"), @r"
    success: true
    exit_code: 0
    ----- stdout -----
    An integrated language service for Typst.

    Usage: tinymist [COMMAND]

    Commands:
      probe       Probes existence (Nop run)
      completion  Generates completion script to stdout
      lsp         Runs language server
      dap         Runs debug adapter
      preview     Runs preview server
      test        Test a document and gives summary
      compile     Runs compile command like `typst-cli compile`
      help        Print this message or the help of the given subcommand(s)

    Options:
      -h, --help     Print help
      -V, --version  Print version

    ----- stderr -----
    ");
}

#[test]
fn test_help_lsp() {
    apply_common_filters!();
    insta_cmd::assert_cmd_snapshot!(cli().arg("lsp").arg("--help"), @r"
    success: true
    exit_code: 0
    ----- stdout -----
    Runs language server

    Usage: tinymist lsp [OPTIONS]

    Options:
          --mirror <FILE>        Mirror the stdin to the file [default: ]
          --replay <FILE>        Replay input from the file [default: ]
          --font-path <DIR>      Font paths [env: TYPST_FONT_PATHS=REDACTED]
          --ignore-system-fonts  Ensures system fonts won't be searched, unless explicitly included via
                                 `--font-path`
      -h, --help                 Print help

    ----- stderr -----
    ");
}

#[test]
fn test_help_compile() {
    apply_common_filters!();
    insta_cmd::assert_cmd_snapshot!(cli().arg("compile").arg("--help"), @r"
    success: true
    exit_code: 0
    ----- stdout -----
    Runs compile command like `typst-cli compile`

    Usage: tinymist compile [OPTIONS] <INPUT> [OUTPUT]

    Arguments:
      <INPUT>
              Path to input Typst file

      [OUTPUT]
              Path to output file (PDF, PNG, SVG, or HTML). Use `-` to write output to stdout.
              
              For output formats emitting one file per page (PNG & SVG), a page number template must be
              present if the source document renders to multiple pages. Use `{p}` for page numbers,
              `{0p}` for zero padded page numbers and `{t}` for page count. For example,
              `page-{0p}-of-{t}.png` creates `page-01-of-10.png`, `page-02-of-10.png`, and so on.

    Options:
          --name <NAME>
              Give a name to the document

          --root <DIR>
              Configures the project root (for absolute paths)
              
              [env: TYPST_ROOT=REDACTED]

          --font-path <DIR>
              Font paths
              
              [env: TYPST_FONT_PATHS=REDACTED]

          --ignore-system-fonts
              Ensures system fonts won't be searched, unless explicitly included via `--font-path`

          --package-path <DIR>
              Custom path to local packages, defaults to system-dependent location
              
              [env: TYPST_PACKAGE_PATH=REDACTED]

          --package-cache-path <DIR>
              Custom path to package cache, defaults to system-dependent location
              
              [env: TYPST_PACKAGE_CACHE_PATH=REDACTED]

          --task <TASK_NAME>
              Name a task

          --when <WHEN>
              When to run the task

              Possible values:
              - never:              Never watch to run task
              - onSave:             Run task on saving the document, i.e. on `textDocument/didSave`
                events
              - onType:             Run task on typing, i.e. on `textDocument/didChange` events
              - onDocumentHasTitle: *DEPRECATED* Run task when a document has a title and on saved,
                which is useful to filter out template files
              - script:             Checks by running a typst script

      -f, --format <FORMAT>
              The format of the output file, inferred from the extension by default

              Possible values:
              - pdf:  Export to PDF
              - png:  Export to PNG
              - svg:  Export to SVG
              - html: Export to HTML

          --pages <PAGES>
              Which pages to export. When unspecified, all pages are exported.
              
              Pages to export are separated by commas, and can be either simple page numbers (e.g. '2,5'
              to export only pages 2 and 5) or page ranges (e.g. '2,3-6,8-' to export page 2, pages 3 to
              6 (inclusive), page 8 and any pages after it).
              
              Page numbers are one-indexed and correspond to physical page numbers in the document
              (therefore not being affected by the document's page counter).

          --pdf-standard <PDF_STANDARD>
              One (or multiple comma-separated) PDF standards that Typst will enforce conformance with

              Possible values:
              - 1.7:  PDF 1.7
              - a-2b: PDF/A-2b
              - a-3b: PDF/A-3b

          --ppi <PPI>
              The PPI (pixels per inch) to use for PNG export
              
              [default: 144]

          --save-lock
              Saves the compilation arguments to the lock file

          --lockfile <LOCKFILE>
              Specifies the path to the lock file. If the path is set, the lock file will be saved

      -h, --help
              Print help (see a summary with '-h')

    ----- stderr -----
    ");
}

#[test]
fn test_help_preview() {
    apply_common_filters!();
    insta_cmd::assert_cmd_snapshot!(cli().arg("preview").arg("--help"), @r#"
    success: true
    exit_code: 0
    ----- stdout -----
    Runs preview server

    Usage: tinymist preview [OPTIONS] [INPUT]

    Arguments:
      [INPUT]
              Path to input Typst file

    Options:
          --preview-mode <MODE>
              Preview mode

              Possible values:
              - document: Preview mode for regular document
              - slide:    Preview mode for slide
              
              [default: document]

          --partial-rendering <ENABLE_PARTIAL_RENDERING>
              Only render visible part of the document. This can improve performance but still being
              experimental
              
              [possible values: true, false]

          --invert-colors <INVERT_COLORS>
              Invert colors of the preview (useful for dark themes without cost). Please note you could
              see the origin colors when you hover elements in the preview.
              
              It is also possible to specify strategy to each element kind by an object map in JSON
              format.
              
              Possible element kinds: - `image`: Images in the preview. - `rest`: Rest elements in the
              preview.
              
              By default, the preview will never invert colors.
              
              ## Example
              
              By string:
              
              ```shell --invert-colors=auto ```
              
              By element:
              
              ```shell --invert-colors='{"rest": "always", "image": "never"}' ```

          --root <DIR>
              Configures the project root (for absolute paths)

          --font-path <DIR>
              Font paths
              
              [env: TYPST_FONT_PATHS=REDACTED]

          --ignore-system-fonts
              Ensures system fonts won't be searched, unless explicitly included via `--font-path`

          --package-path <DIR>
              Custom path to local packages, defaults to system-dependent location
              
              [env: TYPST_PACKAGE_PATH=REDACTED]

          --package-cache-path <DIR>
              Custom path to package cache, defaults to system-dependent location
              
              [env: TYPST_PACKAGE_CACHE_PATH=REDACTED]

          --features <FEATURES>
              Enables in-development features that may be changed or removed at any time
              
              [env: TYPST_FEATURES=REDACTED]
              [possible values: html]

          --input <key=value>
              Add a string key-value pair visible through `sys.inputs`

          --pdf-standard <PDF_STANDARD>
              One (or multiple comma-separated) PDF standards that Typst will enforce conformance with

              Possible values:
              - 1.7:  PDF 1.7
              - a-2b: PDF/A-2b
              - a-3b: PDF/A-3b

          --cert <CERT_PATH>
              Path to CA certificate file for network access, especially for downloading typst packages
              
              [env: TYPST_CERT=REDACTED]

          --host <HOST>
              (Deprecated) (File) Host for the preview server. Note: if it equals to `data_plane_host`,
              same address will be used
              
              [default: ]

          --open
              Open the preview in the browser after compilation. If `--no-open` is set, this flag will
              be ignored

          --no-open
              Don't open the preview in the browser after compilation. If `--open` is set as well, this
              flag will win

      -h, --help
              Print help (see a summary with '-h')

    ----- stderr -----
    "#);
}
#[test]
fn test_compile() {
    const INPUT_REL: &str = "tests/workspaces/individuals/tiny.typ";

    std::env::set_var("RUST_BACKTRACE", "full");
    let cwd = GIT_ROOT.clone();
    let root = cwd.join("target/e2e/tinymist-cli");

    std::env::set_current_dir(&cwd).expect("should change current directory");
    tinymist_std::fs::paths::temp_dir_in(root, |tmp| {
        let abs_out = tmp.clean();
        let rel_out = abs_out.strip_prefix(&cwd).expect("path should be stripped");

        assert!(cwd.is_absolute(), "cwd should be absolute {cwd:?}");
        assert!(abs_out.is_absolute(), "abs_out should be absolute {abs_out:?}");
        assert!(rel_out.is_relative(), "rel_out should be relative {rel_out:?}");

        // absolute INPUT, absolute OUTPUT
        insta_cmd::assert_cmd_snapshot!(cli().arg("compile").arg(cwd.join(INPUT_REL)).arg(abs_out.join("test1.pdf")), @r"
        success: true
        exit_code: 0
        ----- stdout -----

        ----- stderr -----
        ");
        // absolute INPUT, relative OUTPUT
        insta_cmd::assert_cmd_snapshot!(cli().arg("compile").arg(cwd.join(INPUT_REL)).arg(rel_out.join("test2.pdf")), @r"
        success: true
        exit_code: 0
        ----- stdout -----

        ----- stderr -----
        ");
        // relative INPUT, absolute OUTPUT
        insta_cmd::assert_cmd_snapshot!(cli().arg("compile").arg(INPUT_REL).arg(abs_out.join("test3.pdf")), @r"
        success: true
        exit_code: 0
        ----- stdout -----

        ----- stderr -----
        ");
        // relative INPUT, relative OUTPUT
        insta_cmd::assert_cmd_snapshot!(cli().arg("compile").arg(INPUT_REL).arg(rel_out.join("test4.pdf")), @r"
        success: true
        exit_code: 0
        ----- stdout -----

        ----- stderr -----
        ");

        for i in 1..=4 {
            let output = rel_out.join(format!("test{i}.pdf"));
            assert!(output.exists(), "output file should exist: {output:?}");
        }

        Ok(())
    })
    .expect("test should succeed");
}
