#![allow(clippy::comparison_chain)]

use std::cell::{RefCell, RefMut};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::{env, io};

use clap::Parser;
use comemo::{Prehashed, Track};
use elsa::FrozenVec;
use once_cell::unsync::OnceCell;
use oxipng::{InFile, Options, OutFile};
use rayon::iter::{ParallelBridge, ParallelIterator};
use tiny_skia as sk;
use unscanny::Scanner;
use walkdir::WalkDir;

use typst::diag::{bail, FileError, FileResult, StrResult};
use typst::doc::{Document, Frame, FrameItem, Meta};
use typst::eval::{func, Datetime, Library, NoneValue, Value};
use typst::font::{Font, FontBook};
use typst::geom::{Abs, Color, RgbaColor, Sides, Smart};
use typst::syntax::{Source, SourceId, Span, SyntaxNode};
use typst::util::{Buffer, PathExt};
use typst::World;
use typst_library::layout::PageElem;
use typst_library::text::{TextElem, TextSize};

const TYP_DIR: &str = "typ";
const REF_DIR: &str = "ref";
const PNG_DIR: &str = "png";
const PDF_DIR: &str = "pdf";
const FONT_DIR: &str = "../assets/fonts";
const FILE_DIR: &str = "../assets/files";

#[derive(Debug, Clone, Parser)]
#[clap(name = "typst-test", author)]
struct Args {
    filter: Vec<String>,
    /// runs only the specified subtest
    #[arg(short, long)]
    #[arg(allow_hyphen_values = true)]
    subtest: Option<isize>,
    #[arg(long)]
    exact: bool,
    #[arg(long, default_value_t = env::var_os("UPDATE_EXPECT").is_some())]
    update: bool,
    #[arg(long)]
    pdf: bool,
    #[command(flatten)]
    print: PrintConfig,
    #[arg(long)]
    nocapture: bool, // simply ignores the argument
}

/// Which things to print out for debugging.
#[derive(Default, Debug, Copy, Clone, Eq, PartialEq, Parser)]
struct PrintConfig {
    #[arg(long)]
    syntax: bool,
    #[arg(long)]
    model: bool,
    #[arg(long)]
    frames: bool,
}

impl Args {
    fn matches(&self, path: &Path) -> bool {
        if self.exact {
            let name = path.file_name().unwrap().to_string_lossy();
            self.filter.iter().any(|v| v == &name)
        } else {
            let path = path.to_string_lossy();
            self.filter.is_empty() || self.filter.iter().any(|v| path.contains(v))
        }
    }
}

fn main() {
    let args = Args::parse();

    // Create loader and context.
    let world = TestWorld::new(args.print);

    println!("Running tests...");
    let results = WalkDir::new("typ")
        .into_iter()
        .par_bridge()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            if entry.depth() == 0 {
                return None;
            }

            if entry.path().starts_with("typ/benches") {
                return None;
            }

            let src_path = entry.into_path();
            if src_path.extension() != Some(OsStr::new("typ")) {
                return None;
            }

            if args.matches(&src_path) {
                Some(src_path)
            } else {
                None
            }
        })
        .map_with(world, |world, src_path| {
            let path = src_path.strip_prefix(TYP_DIR).unwrap();
            let png_path = Path::new(PNG_DIR).join(path).with_extension("png");
            let ref_path = Path::new(REF_DIR).join(path).with_extension("png");
            let pdf_path =
                args.pdf.then(|| Path::new(PDF_DIR).join(path).with_extension("pdf"));

            test(world, &src_path, &png_path, &ref_path, pdf_path.as_deref(), &args)
                as usize
        })
        .collect::<Vec<_>>();

    let len = results.len();
    let ok = results.iter().sum::<usize>();
    if len > 1 {
        println!("{ok} / {len} tests passed.");
    }

    if ok != len {
        println!(
            "Set the UPDATE_EXPECT environment variable or pass the \
             --update flag to update the reference image(s)."
        );
    }

    if ok < len {
        std::process::exit(1);
    }
}

fn library() -> Library {
    /// Display: Test
    /// Category: test
    #[func]
    fn test(lhs: Value, rhs: Value) -> StrResult<NoneValue> {
        if lhs != rhs {
            bail!("Assertion failed: {lhs:?} != {rhs:?}");
        }
        Ok(NoneValue)
    }

    /// Display: Print
    /// Category: test
    #[func]
    fn print(#[variadic] values: Vec<Value>) -> NoneValue {
        let mut stdout = io::stdout().lock();
        write!(stdout, "> ").unwrap();
        for (i, value) in values.into_iter().enumerate() {
            if i > 0 {
                write!(stdout, ", ").unwrap();
            }
            write!(stdout, "{value:?}").unwrap();
        }
        writeln!(stdout).unwrap();
        NoneValue
    }

    let mut lib = typst_library::build();

    // Set page width to 120pt with 10pt margins, so that the inner page is
    // exactly 100pt wide. Page height is unbounded and font size is 10pt so
    // that it multiplies to nice round numbers.
    lib.styles
        .set(PageElem::set_width(Smart::Custom(Abs::pt(120.0).into())));
    lib.styles.set(PageElem::set_height(Smart::Auto));
    lib.styles.set(PageElem::set_margin(Sides::splat(Some(Smart::Custom(
        Abs::pt(10.0).into(),
    )))));
    lib.styles.set(TextElem::set_size(TextSize(Abs::pt(10.0).into())));

    // Hook up helpers into the global scope.
    lib.global.scope_mut().define("test", test_func());
    lib.global.scope_mut().define("print", print_func());
    lib.global
        .scope_mut()
        .define("conifer", RgbaColor::new(0x9f, 0xEB, 0x52, 0xFF));
    lib.global
        .scope_mut()
        .define("forest", RgbaColor::new(0x43, 0xA1, 0x27, 0xFF));

    lib
}

/// A world that provides access to the tests environment.
struct TestWorld {
    print: PrintConfig,
    library: Prehashed<Library>,
    book: Prehashed<FontBook>,
    fonts: Vec<Font>,
    paths: RefCell<HashMap<PathBuf, PathSlot>>,
    sources: FrozenVec<Box<Source>>,
    main: SourceId,
}

impl Clone for TestWorld {
    fn clone(&self) -> Self {
        Self {
            print: self.print,
            library: self.library.clone(),
            book: self.book.clone(),
            fonts: self.fonts.clone(),
            paths: self.paths.clone(),
            sources: FrozenVec::from_iter(self.sources.iter().cloned().map(Box::new)),
            main: self.main,
        }
    }
}

#[derive(Default, Clone)]
struct PathSlot {
    source: OnceCell<FileResult<SourceId>>,
    buffer: OnceCell<FileResult<Buffer>>,
}

impl TestWorld {
    fn new(print: PrintConfig) -> Self {
        // Search for fonts.
        let mut fonts = vec![];
        for entry in WalkDir::new(FONT_DIR)
            .sort_by_file_name()
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|entry| entry.file_type().is_file())
        {
            let data = fs::read(entry.path()).unwrap();
            fonts.extend(Font::iter(data.into()));
        }

        Self {
            print,
            library: Prehashed::new(library()),
            book: Prehashed::new(FontBook::from_fonts(&fonts)),
            fonts,
            paths: RefCell::default(),
            sources: FrozenVec::new(),
            main: SourceId::detached(),
        }
    }
}

impl World for TestWorld {
    fn root(&self) -> &Path {
        Path::new(FILE_DIR)
    }

    fn library(&self) -> &Prehashed<Library> {
        &self.library
    }

    fn main(&self) -> &Source {
        self.source(self.main)
    }

    fn resolve(&self, path: &Path) -> FileResult<SourceId> {
        self.slot(path)
            .source
            .get_or_init(|| {
                let buf = read(path)?;
                let text = String::from_utf8(buf)?;
                Ok(self.insert(path, text))
            })
            .clone()
    }

    fn source(&self, id: SourceId) -> &Source {
        &self.sources[id.as_u16() as usize]
    }

    fn book(&self) -> &Prehashed<FontBook> {
        &self.book
    }

    fn font(&self, id: usize) -> Option<Font> {
        Some(self.fonts[id].clone())
    }

    fn file(&self, path: &Path) -> FileResult<Buffer> {
        self.slot(path)
            .buffer
            .get_or_init(|| read(path).map(Buffer::from))
            .clone()
    }

    fn today(&self, _: Option<i64>) -> Option<Datetime> {
        Some(Datetime::from_ymd(1970, 1, 1).unwrap())
    }
}

impl TestWorld {
    fn set(&mut self, path: &Path, text: String) -> SourceId {
        let slot = self.slot(path);
        let id = if let Some(&Ok(id)) = slot.source.get() {
            drop(slot);
            self.sources.as_mut()[id.as_u16() as usize].replace(text);
            id
        } else {
            let id = self.insert(path, text);
            slot.source.set(Ok(id)).unwrap();
            drop(slot);
            id
        };
        self.main = id;
        id
    }

    fn slot(&self, path: &Path) -> RefMut<PathSlot> {
        RefMut::map(self.paths.borrow_mut(), |paths| {
            paths.entry(path.normalize()).or_default()
        })
    }

    fn insert(&self, path: &Path, text: String) -> SourceId {
        let id = SourceId::from_u16(self.sources.len() as u16);
        let source = Source::new(id, path, text);
        self.sources.push(Box::new(source));
        id
    }
}

/// Read as file.
fn read(path: &Path) -> FileResult<Vec<u8>> {
    let suffix = path
        .strip_prefix(FILE_DIR)
        .map(|suffix| Path::new("/").join(suffix))
        .unwrap_or_else(|_| path.into());

    let f = |e| FileError::from_io(e, &suffix);
    if fs::metadata(path).map_err(f)?.is_dir() {
        Err(FileError::IsDirectory)
    } else {
        fs::read(path).map_err(f)
    }
}

fn test(
    world: &mut TestWorld,
    src_path: &Path,
    png_path: &Path,
    ref_path: &Path,
    pdf_path: Option<&Path>,
    args: &Args,
) -> bool {
    struct PanicGuard<'a>(&'a Path);
    impl Drop for PanicGuard<'_> {
        fn drop(&mut self) {
            if std::thread::panicking() {
                println!("Panicked in {}", self.0.display());
            }
        }
    }

    let name = src_path.strip_prefix(TYP_DIR).unwrap_or(src_path);
    let text = fs::read_to_string(src_path).unwrap();
    let _guard = PanicGuard(name);

    let mut output = String::new();
    let mut ok = true;
    let mut updated = false;
    let mut frames = vec![];
    let mut line = 0;
    let mut compare_ref = true;
    let mut compare_ever = false;
    let mut rng = LinearShift::new();

    let parts: Vec<_> = text
        .split("\n---")
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .collect();

    for (i, &part) in parts.iter().enumerate() {
        if let Some(x) = args.subtest {
            let x = usize::try_from(
                x.rem_euclid(isize::try_from(parts.len()).unwrap_or_default()),
            )
            .unwrap();
            if x != i {
                writeln!(output, "  Skipped subtest {i}.").unwrap();
                continue;
            }
        }
        let is_header = i == 0
            && parts.len() > 1
            && part
                .lines()
                .all(|s| s.starts_with("//") || s.chars().all(|c| c.is_whitespace()));

        if is_header {
            for line in part.lines() {
                if line.starts_with("// Ref: false") {
                    compare_ref = false;
                }
            }
        } else {
            let (part_ok, compare_here, part_frames) = test_part(
                &mut output,
                world,
                src_path,
                part.into(),
                i,
                compare_ref,
                line,
                &mut rng,
            );

            ok &= part_ok;
            compare_ever |= compare_here;
            frames.extend(part_frames);
        }

        line += part.lines().count() + 1;
    }

    let document = Document { pages: frames, ..Default::default() };
    if compare_ever {
        if let Some(pdf_path) = pdf_path {
            let pdf_data = typst::export::pdf(&document);
            fs::create_dir_all(pdf_path.parent().unwrap()).unwrap();
            fs::write(pdf_path, pdf_data).unwrap();
        }

        if world.print.frames {
            for frame in &document.pages {
                writeln!(output, "{:#?}\n", frame).unwrap();
            }
        }

        let canvas = render(&document.pages);
        fs::create_dir_all(png_path.parent().unwrap()).unwrap();
        canvas.save_png(png_path).unwrap();

        if let Ok(ref_pixmap) = sk::Pixmap::load_png(ref_path) {
            if canvas.width() != ref_pixmap.width()
                || canvas.height() != ref_pixmap.height()
                || canvas
                    .data()
                    .iter()
                    .zip(ref_pixmap.data())
                    .any(|(&a, &b)| a.abs_diff(b) > 2)
            {
                if args.update {
                    update_image(png_path, ref_path);
                    updated = true;
                } else {
                    writeln!(output, "  Does not match reference image.").unwrap();
                    ok = false;
                }
            }
        } else if !document.pages.is_empty() {
            if args.update {
                update_image(png_path, ref_path);
                updated = true;
            } else {
                writeln!(output, "  Failed to open reference image.").unwrap();
                ok = false;
            }
        }
    }

    {
        let mut stdout = io::stdout().lock();
        stdout.write_all(name.to_string_lossy().as_bytes()).unwrap();
        if ok {
            writeln!(stdout, " ✔").unwrap();
        } else {
            writeln!(stdout, " ❌").unwrap();
        }
        if updated {
            writeln!(stdout, "  Updated reference image.").unwrap();
        }
        if !output.is_empty() {
            stdout.write_all(output.as_bytes()).unwrap();
        }
    }

    ok
}

fn update_image(png_path: &Path, ref_path: &Path) {
    oxipng::optimize(
        &InFile::Path(png_path.to_owned()),
        &OutFile::Path(Some(ref_path.to_owned())),
        &Options::max_compression(),
    )
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn test_part(
    output: &mut String,
    world: &mut TestWorld,
    src_path: &Path,
    text: String,
    i: usize,
    compare_ref: bool,
    line: usize,
    rng: &mut LinearShift,
) -> (bool, bool, Vec<Frame>) {
    let mut ok = true;

    let id = world.set(src_path, text);
    let source = world.source(id);
    if world.print.syntax {
        writeln!(output, "Syntax Tree:\n{:#?}\n", source.root()).unwrap();
    }

    let (local_compare_ref, mut ref_errors) = parse_metadata(source);
    let compare_ref = local_compare_ref.unwrap_or(compare_ref);

    ok &= test_spans(output, source.root());
    ok &= test_reparse(output, world.source(id).text(), i, rng);

    if world.print.model {
        let world = (world as &dyn World).track();
        let route = typst::eval::Route::default();
        let mut tracer = typst::eval::Tracer::default();
        let module =
            typst::eval::eval(world, route.track(), tracer.track_mut(), source).unwrap();
        writeln!(output, "Model:\n{:#?}\n", module.content()).unwrap();
    }

    let (mut frames, errors) = match typst::compile(world) {
        Ok(document) => (document.pages, vec![]),
        Err(errors) => (vec![], *errors),
    };

    // Don't retain frames if we don't wanna compare with reference images.
    if !compare_ref {
        frames.clear();
    }

    // Map errors to range and message format, discard traces and errors from
    // other files.
    let mut errors: Vec<_> = errors
        .into_iter()
        .filter(|error| error.span.source() == id)
        .map(|error| (error.range(world), error.message.replace('\\', "/")))
        .collect();

    errors.sort_by_key(|error| error.0.start);
    ref_errors.sort_by_key(|error| error.0.start);

    if errors != ref_errors {
        writeln!(output, "  Subtest {i} does not match expected errors.").unwrap();
        ok = false;

        let source = world.source(id);
        for error in errors.iter() {
            if !ref_errors.contains(error) {
                write!(output, "    Not annotated | ").unwrap();
                print_error(output, source, line, error);
            }
        }

        for error in ref_errors.iter() {
            if !errors.contains(error) {
                write!(output, "    Not emitted   | ").unwrap();
                print_error(output, source, line, error);
            }
        }
    }

    (ok, compare_ref, frames)
}

fn parse_metadata(source: &Source) -> (Option<bool>, Vec<(Range<usize>, String)>) {
    let mut compare_ref = None;
    let mut errors = vec![];

    let lines: Vec<_> = source.text().lines().map(str::trim).collect();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("// Ref: false") {
            compare_ref = Some(false);
        }

        if line.starts_with("// Ref: true") {
            compare_ref = Some(true);
        }

        fn num(s: &mut Scanner) -> usize {
            s.eat_while(char::is_numeric).parse().unwrap()
        }

        let comments =
            lines[i..].iter().take_while(|line| line.starts_with("//")).count();

        let pos = |s: &mut Scanner| -> usize {
            let first = num(s) - 1;
            let (delta, column) =
                if s.eat_if(':') { (first, num(s) - 1) } else { (0, first) };
            let line = (i + comments) + delta;
            source.line_column_to_byte(line, column).unwrap()
        };

        let Some(rest) = line.strip_prefix("// Error: ") else { continue; };
        let mut s = Scanner::new(rest);
        let start = pos(&mut s);
        let end = if s.eat_if('-') { pos(&mut s) } else { start };
        let range = start..end;

        errors.push((range, s.after().trim().to_string()));
    }

    (compare_ref, errors)
}

fn print_error(
    output: &mut String,
    source: &Source,
    line: usize,
    (range, message): &(Range<usize>, String),
) {
    let start_line = 1 + line + source.byte_to_line(range.start).unwrap();
    let start_col = 1 + source.byte_to_column(range.start).unwrap();
    let end_line = 1 + line + source.byte_to_line(range.end).unwrap();
    let end_col = 1 + source.byte_to_column(range.end).unwrap();
    writeln!(output, "Error: {start_line}:{start_col}-{end_line}:{end_col}: {message}")
        .unwrap();
}

/// Pseudorandomly edit the source file and test whether a reparse produces the
/// same result as a clean parse.
///
/// The method will first inject 10 strings once every 400 source characters
/// and then select 5 leaf node boundaries to inject an additional, randomly
/// chosen string from the injection list.
fn test_reparse(
    output: &mut String,
    text: &str,
    i: usize,
    rng: &mut LinearShift,
) -> bool {
    let supplements = [
        "[",
        "]",
        "{",
        "}",
        "(",
        ")",
        "#rect()",
        "a word",
        ", a: 1",
        "10.0",
        ":",
        "if i == 0 {true}",
        "for",
        "* hello *",
        "//",
        "/*",
        "\\u{12e4}",
        "```typst",
        " ",
        "trees",
        "\\",
        "$ a $",
        "2.",
        "-",
        "5",
    ];

    let mut ok = true;

    let mut apply = |replace: Range<usize>, with| {
        let mut incr_source = Source::detached(text);
        if incr_source.root().len() != text.len() {
            println!(
                "    Subtest {i} tree length {} does not match string length {} ❌",
                incr_source.root().len(),
                text.len(),
            );
            return false;
        }

        incr_source.edit(replace.clone(), with);

        let edited_src = incr_source.text();
        let ref_source = Source::detached(edited_src);
        let mut ref_root = ref_source.root().clone();
        let mut incr_root = incr_source.root().clone();

        // Ensures that the span numbering invariants hold.
        let spans_ok = test_spans(output, &ref_root) && test_spans(output, &incr_root);

        // Remove all spans so that the comparison works out.
        let tree_ok = {
            ref_root.synthesize(Span::detached());
            incr_root.synthesize(Span::detached());
            ref_root == incr_root
        };

        if !tree_ok {
            writeln!(
                output,
                "    Subtest {i} reparse differs from clean parse when inserting '{with}' at {}-{} ❌\n",
                replace.start, replace.end,
            ).unwrap();
            writeln!(output, "    Expected reference tree:\n{ref_root:#?}\n").unwrap();
            writeln!(output, "    Found incremental tree:\n{incr_root:#?}").unwrap();
            writeln!(
                output,
                "    Full source ({}):\n\"{edited_src:?}\"",
                edited_src.len()
            )
            .unwrap();
        }

        spans_ok && tree_ok
    };

    let mut pick = |range: Range<usize>| {
        let ratio = rng.next();
        (range.start as f64 + ratio * (range.end - range.start) as f64).floor() as usize
    };

    let insertions = (text.len() as f64 / 400.0).ceil() as usize;
    for _ in 0..insertions {
        let supplement = supplements[pick(0..supplements.len())];
        let start = pick(0..text.len());
        let end = pick(start..text.len());

        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }

        ok &= apply(start..end, supplement);
    }

    let source = Source::detached(text);
    let leafs = leafs(source.root());
    let start = source.range(leafs[pick(0..leafs.len())].span()).start;
    let supplement = supplements[pick(0..supplements.len())];
    ok &= apply(start..start, supplement);

    ok
}

/// Returns all leaf descendants of a node (may include itself).
fn leafs(node: &SyntaxNode) -> Vec<SyntaxNode> {
    if node.children().len() == 0 {
        vec![node.clone()]
    } else {
        node.children().flat_map(leafs).collect()
    }
}

/// Ensure that all spans are properly ordered (and therefore unique).
#[track_caller]
fn test_spans(output: &mut String, root: &SyntaxNode) -> bool {
    test_spans_impl(output, root, 0..u64::MAX)
}

#[track_caller]
fn test_spans_impl(output: &mut String, node: &SyntaxNode, within: Range<u64>) -> bool {
    if !within.contains(&node.span().number()) {
        writeln!(output, "    Node: {node:#?}").unwrap();
        writeln!(
            output,
            "    Wrong span order: {} not in {within:?} ❌",
            node.span().number()
        )
        .unwrap();
    }

    let start = node.span().number() + 1;
    let mut children = node.children().peekable();
    while let Some(child) = children.next() {
        let end = children.peek().map_or(within.end, |next| next.span().number());
        if !test_spans_impl(output, child, start..end) {
            return false;
        }
    }

    true
}

/// Draw all frames into one image with padding in between.
fn render(frames: &[Frame]) -> sk::Pixmap {
    let pixel_per_pt = 2.0;
    let pixmaps: Vec<_> = frames
        .iter()
        .map(|frame| {
            let limit = Abs::cm(100.0);
            if frame.width() > limit || frame.height() > limit {
                panic!("overlarge frame: {:?}", frame.size());
            }
            typst::export::render(frame, pixel_per_pt, Color::WHITE)
        })
        .collect();

    let pad = (5.0 * pixel_per_pt).round() as u32;
    let pxw = 2 * pad + pixmaps.iter().map(sk::Pixmap::width).max().unwrap_or_default();
    let pxh = pad + pixmaps.iter().map(|pixmap| pixmap.height() + pad).sum::<u32>();

    let mut canvas = sk::Pixmap::new(pxw, pxh).unwrap();
    canvas.fill(sk::Color::BLACK);

    let [x, mut y] = [pad; 2];
    for (frame, mut pixmap) in frames.iter().zip(pixmaps) {
        let ts = sk::Transform::from_scale(pixel_per_pt, pixel_per_pt);
        render_links(&mut pixmap, ts, frame);

        canvas.draw_pixmap(
            x as i32,
            y as i32,
            pixmap.as_ref(),
            &sk::PixmapPaint::default(),
            sk::Transform::identity(),
            None,
        );

        y += pixmap.height() + pad;
    }

    canvas
}

/// Draw extra boxes for links so we can see whether they are there.
fn render_links(canvas: &mut sk::Pixmap, ts: sk::Transform, frame: &Frame) {
    for (pos, item) in frame.items() {
        let ts = ts.pre_translate(pos.x.to_pt() as f32, pos.y.to_pt() as f32);
        match *item {
            FrameItem::Group(ref group) => {
                let ts = ts.pre_concat(group.transform.into());
                render_links(canvas, ts, &group.frame);
            }
            FrameItem::Meta(Meta::Link(_), size) => {
                let w = size.x.to_pt() as f32;
                let h = size.y.to_pt() as f32;
                let rect = sk::Rect::from_xywh(0.0, 0.0, w, h).unwrap();
                let mut paint = sk::Paint::default();
                paint.set_color_rgba8(40, 54, 99, 40);
                canvas.fill_rect(rect, &paint, ts, None);
            }
            _ => {}
        }
    }
}

/// A Linear-feedback shift register using XOR as its shifting function.
/// Can be used as PRNG.
struct LinearShift(u64);

impl LinearShift {
    /// Initialize the shift register with a pre-set seed.
    pub fn new() -> Self {
        Self(0xACE5)
    }

    /// Return a pseudo-random number between `0.0` and `1.0`.
    pub fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 3;
        self.0 ^= self.0 << 14;
        self.0 ^= self.0 >> 28;
        self.0 ^= self.0 << 36;
        self.0 ^= self.0 >> 52;
        self.0 as f64 / u64::MAX as f64
    }
}
