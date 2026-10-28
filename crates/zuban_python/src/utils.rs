use std::{
    borrow::{Borrow, Cow},
    cell::Cell,
    collections::HashMap,
    fmt,
    hash::{Hash, Hasher},
    sync::Arc,
};

use parsa_python_cst::{Name, NodeIndex, SourceOwner, Tree};

thread_local!(pub(crate) static DEBUG_INDENTATION: Cell<usize> = const { Cell::new(0) });

#[inline(always)]
#[must_use]
pub(crate) fn debug_indent() -> DebugIndent {
    if cfg!(feature = "zuban_debug") {
        DEBUG_INDENTATION.with(|i| {
            i.set(i.get() + 1);
        })
    }
    DebugIndent()
}

pub(crate) struct DebugIndent();

impl Drop for DebugIndent {
    #[inline(always)]
    fn drop(&mut self) {
        if cfg!(feature = "zuban_debug") {
            DEBUG_INDENTATION.with(|i| i.set(i.get() - 1));
        }
    }
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        if cfg!(feature="zuban_debug") {
            use std::iter::repeat;
            let indent = $crate::utils::DEBUG_INDENTATION.with(|i| i.get());
            print!("{}", repeat(' ').take(indent).collect::<String>());
            println!($($arg)*);
        }
    }
}

#[macro_export]
macro_rules! recoverable_error {
    ($($arg:tt)*) => {
        if std::env::var("ZUBAN_CRASH_ON_ERROR").is_ok_and(|s| s == "1") {
            panic!($($arg)*)
        } else {
            tracing::error!($($arg)*);
            tracing::error!("To crash on this error, use set the environment variable ZUBAN_CRASH_ON_ERROR=1");
        }
    }
}

#[macro_export]
macro_rules! new_class {
    ($link:expr, $($arg:expr),+$(,)?) => {
        $crate::type_::Type::new_class(
            $link,
            $crate::type_::ClassGenerics::List(
                $crate::type_::GenericsList::new_generics(std::sync::Arc::new([
                    $($crate::type_::Generic::TypeArg($arg)),*
                ]))
            )
        )
    }
}

#[derive(Clone)]
struct HashableRawStr {
    ptr: *const str,
}

impl HashableRawStr {
    fn new(string: &str) -> Self {
        Self { ptr: string }
    }

    fn as_str(&self) -> &str {
        // Stored keys point into SymbolTable::source. Lookup keys borrow their
        // input only for the duration of HashMap::get.
        unsafe { &*self.ptr }
    }
}

// The pointed-to source is immutable and owned by the containing SymbolTable.
// Transient lookup keys are used only within the lookup call.
unsafe impl Sync for HashableRawStr {}
unsafe impl Send for HashableRawStr {}

impl Hash for HashableRawStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialEq for HashableRawStr {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for HashableRawStr {}

impl fmt::Debug for HashableRawStr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

#[derive(Clone)]
pub(crate) struct SymbolTable {
    // The name symbol table comes from compiler theory, it's basically a mapping of a name to a
    // pointer. To avoid wasting space, we don't use a pointer here, instead we use the node index,
    // which acts as one.
    symbols: HashMap<HashableRawStr, NodeIndex>,
    // HashableRawStr borrows source bytes. Keep their owner alive even when a
    // symbol table is cloned or moved independently of its PythonFile.
    source: SourceOwner,
}

impl SymbolTable {
    pub fn new(source: Tree) -> Self {
        Self {
            symbols: HashMap::new(),
            source: source.source_owner(),
        }
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &NodeIndex)> {
        // This should only ever be called on a table that is not still mutated.
        self.symbols.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn add_or_replace_symbol(&mut self, name: Name) -> Option<NodeIndex> {
        let stored_name = &self.source.code()[name.start() as usize..name.end() as usize];
        debug_assert_eq!(name.as_str(), stored_name);
        self.symbols
            .insert(HashableRawStr::new(stored_name), name.index())
    }

    pub fn lookup_symbol(&self, name: &str) -> Option<NodeIndex> {
        self.symbols.get(&HashableRawStr::new(name)).copied()
    }
}

impl fmt::Debug for SymbolTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SymbolTable")
            .field("symbols", &self.symbols)
            .finish()
    }
}

#[cfg(test)]
mod symbol_table_tests {
    use parsa_python_cst::{AssignmentContent, StmtLikeContent, Target};

    use super::*;

    #[test]
    fn symbol_keys_keep_their_source_alive() {
        for (code, symbol) in [("alpha = 1\n", "alpha"), ("π = 1\n", "π")] {
            let tree = Tree::parse(code.into());
            let StmtLikeContent::Assignment(assignment) =
                tree.root().iter_stmt_likes().next().unwrap().node
            else {
                panic!("expected an assignment");
            };
            let AssignmentContent::Normal(mut targets, _) = assignment.unpack() else {
                panic!("expected a normal assignment");
            };
            let Target::Name(name_def) = targets.next().unwrap() else {
                panic!("expected a name target");
            };
            let index = name_def.name_index();
            let mut table = SymbolTable::new(tree.clone());
            table.add_or_replace_symbol(name_def.name());
            drop(tree);

            let detached = table.clone();
            drop(table);
            assert_eq!(detached.source.code(), code);
            assert_eq!(detached.lookup_symbol(symbol), Some(index));
        }
    }
}

pub(crate) fn bytes_repr(bytes: Cow<[u8]>) -> String {
    let mut string = String::with_capacity(bytes.len());
    for &b in bytes.iter() {
        match b {
            b'\t' => string.push_str(r"\t"),
            b'\n' => string.push_str(r"\n"),
            b'\r' => string.push_str(r"\r"),
            b'\\' => string.push_str(r"\\"),
            b' ' => string.push(' '),
            _ if b.is_ascii_graphic() => string.push(b as char),
            _ => string += &format!("\\x{:02x}", b),
        }
    }
    format!("b'{string}'")
}

pub(crate) fn str_repr(content: &str) -> String {
    let mut repr = String::new();
    for c in content.chars() {
        // Since the control characters cause an issue when printing to a terminal, we have to
        // escape this. Some of them also don't have a symbol that would show. Both C0 and C1 ansi
        // escape codes must be escaped.
        if c.is_control() {
            match c {
                '\n' => repr += "\\n",
                '\r' => repr += "\\r",
                '\t' => repr += "\\t",
                _ => {
                    repr += "\\";
                    repr += &format!("{:#04x}", c as u8)[1..];
                }
            }
        } else if c == '\\' {
            repr += r"\\";
        } else {
            repr.push(c);
        }
    }
    format!("'{repr}'")
}

pub(crate) fn join_with_commas<I, S>(input: I) -> String
where
    I: IntoIterator<Item = S>,
    S: Borrow<str>,
{
    input.into_iter().collect::<Vec<S>>().as_slice().join(", ")
}

pub(crate) fn arc_slice_into_vec<T: Clone>(this: Arc<[T]>) -> Vec<T> {
    // Performance issue: Arc -> Vec check https://github.com/rust-lang/rust/issues/93610#issuecomment-1528108612

    // TODO we could avoid cloning here and just use a copy for the slice parts.
    // See also some discussion how this could be done here:
    // https://stackoverflow.com/questions/77511698/rct-try-unwrap-into-vect#comment136989622_77511997
    Vec::from(this.as_ref())
}

#[inline]
pub(crate) fn is_magic_method(name: &str) -> bool {
    name.starts_with("__") && name.ends_with("__")
}

pub(crate) fn is_file_with_python_ending(path_or_name: &str) -> bool {
    path_or_name.ends_with(".py") || path_or_name.ends_with(".pyi")
}

pub(crate) enum EitherIterator<IT1, IT2> {
    Left(IT1),
    Right(IT2),
}

impl<IT1, IT2, T> Iterator for EitherIterator<IT1, IT2>
where
    IT1: Iterator<Item = T>,
    IT2: Iterator<Item = T>,
{
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Left(iterator) => iterator.next(),
            Self::Right(iterator) => iterator.next(),
        }
    }
}

pub(crate) fn limit_length_for_debug(s: &str) -> Cow<'_, str> {
    const MAX_LEN: usize = 30;
    if s.len() > MAX_LEN {
        Cow::Owned(format!("{}..", &s[..MAX_LEN]))
    } else {
        s.into()
    }
}
