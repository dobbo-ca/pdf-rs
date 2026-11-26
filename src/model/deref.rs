//! Dereferencing (Go: pkg/pdfcpu/model/dereference.go, recursion.go:100-130): resolving
//! indirect references, the typed `Dereference*` helpers, reference counting and the lazy
//! decode of object-stream entries.
//!
//! Go's `nil` object is [`Object::Null`]: an undefined or free target, an entry that is not
//! loaded yet and a null entry all dereference to it (7.3.10).
//!
//! Borrowing. A lookup that can decode a lazy object takes `&mut self`, so what it returns
//! (a `&Dict`, `&Array` or `&Object`) borrows the table until it is dropped. The typed scalar
//! helpers return owned values. After the read path has loaded every entry no lazy object is
//! left, and [`XRefTable::peek`] resolves with `&self`.
//!
//! Differences from Go, none of which changes a result the port reaches:
//!
//! * `WithValidationErrorObject` (an error attributed to an object number) is dropped where it
//!   would wrap a cause, so that every predicate of [`Error`] still sees the cause. A scalar
//!   type error with an object number is an [`Error::Validation`].
//! * `DereferenceDestArray` and `DestName` (name trees), `DereferenceForWrite` and
//!   `DereferenceCSVSafeText` are not ported here. The names package or the writer adds them.
//! * `Dereference` of an entry that holds an object stream or an xref stream is an error, since
//!   those are not [`Object`]s here (in Go they are values no type switch of the callers
//!   accepts). Table walks that need them use [`XRefTable::find`].
//! * `dereference_dict_mut` takes a [`Ref`]: a direct dictionary is changed through its owner.

use super::context::Version;
use super::xref::{Entry, EntryObject, XRefTable};
use crate::config::{DEFAULT_MAX_RECURSION_DEPTH, MAX_RECURSION_DEPTH_CEILING};
use crate::error::{Error, Sentinel};
use crate::ledger::Bytes;
use crate::object::string::{hex_decode, unescape};
use crate::object::text::{hex_literal_to_string, string_literal_to_string};
use crate::object::write::Capped;
use crate::object::{Array, Dict, Object, Ref, StreamDict};
use crate::parse::{ParseCtx, parse_object_with_policy};
use std::sync::atomic::AtomicBool;

/// The null object, for lookups that resolve to nothing.
pub(crate) static NULL: Object = Object::Null;

/// The cancel flag of the lookups that have no parameter for one: never set. The decode they
/// can trigger is bounded by `max_decode_bytes` and the ledger (docs/design.md section 3).
pub(crate) static NEVER: AtomicBool = AtomicBool::new(false);

/// Go's `%T` of an object, for messages.
pub(crate) fn type_name(o: &Object) -> &'static str {
    match o {
        Object::Null => "<nil>",
        Object::Bool(_) => "types.Boolean",
        Object::Integer(_) => "types.Integer",
        Object::Float(_) => "types.Float",
        Object::Name(_) => "types.Name",
        Object::String(_) => "types.StringLiteral",
        Object::Hex(_) => "types.HexLiteral",
        Object::Ref(_) => "types.IndirectRef",
        Object::Array(_) => "types.Array",
        Object::Dict(_) => "types.Dict",
        Object::Stream(_) => "types.StreamDict",
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `CheckRecursionDepth`: rejects recursion levels beyond `max_depth` (0 = the default, 100).
///
/// # Errors
/// [`Sentinel::MaxRecursionDepth`].
pub fn check_recursion_depth(name: &str, depth: u64, max_depth: u64) -> Result<(), Error> {
    let max = if max_depth == 0 {
        DEFAULT_MAX_RECURSION_DEPTH
    } else {
        max_depth.min(MAX_RECURSION_DEPTH_CEILING)
    };
    if depth > max {
        return Err(
            Error::Sentinel(Sentinel::MaxRecursionDepth).context(format!(
                "{name}: max recursion depth exceeded (depth {depth}, limit {max})"
            )),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Reference counts

fn walk_dict_ref_counts(x: &mut XRefTable, d: &Dict, depth: u64) -> Result<(), Error> {
    x.check_recursion_depth("reference count traversal", depth)?;
    for (key, e) in d.iter() {
        match e {
            Object::Ref(r) => x.increment_ref_count(*r),
            Object::Dict(_) | Object::Array(_) => {
                walk_ref_counts(x, e, depth + 1)
                    .map_err(|err| err.context(format!("dict entry {}", lossy(key))))?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn walk_array_ref_counts(x: &mut XRefTable, a: &[Object], depth: u64) -> Result<(), Error> {
    x.check_recursion_depth("reference count traversal", depth)?;
    for e in a {
        match e {
            Object::Ref(r) => x.increment_ref_count(*r),
            Object::Dict(_) | Object::Array(_) => walk_ref_counts(x, e, depth + 1)?,
            _ => {}
        }
    }
    Ok(())
}

fn walk_ref_counts(x: &mut XRefTable, o: &Object, depth: u64) -> Result<(), Error> {
    match o {
        Object::Dict(d) => walk_dict_ref_counts(x, d, depth),
        Object::Stream(sd) => walk_dict_ref_counts(x, &sd.dict, depth),
        Object::Array(a) => walk_array_ref_counts(x, a, depth),
        _ => Ok(()),
    }
}

/// `ProcessRefCountsWithError`: adds one to the reference count of every object that `o`
/// refers to, depth first, dictionary keys in order.
///
/// # Errors
/// [`Sentinel::MaxRecursionDepth`]; the counts added before it stay.
pub fn process_ref_counts_with_error(x: &mut XRefTable, o: &Object) -> Result<(), Error> {
    walk_ref_counts(x, o, 0)
}

/// `ProcessRefCounts`: [`process_ref_counts_with_error`] that ignores its error.
pub fn process_ref_counts(x: &mut XRefTable, o: &Object) {
    let _ = walk_ref_counts(x, o, 0);
}

// ---------------------------------------------------------------------------------------------
// Errors

/// `dereferenceDictTypeError`.
fn dict_type_error(raw: &Object, resolved: &Object) -> Error {
    let got = type_name(resolved);
    let msg = match raw {
        Object::Ref(r) => format!(
            "obj#{} gen#{}: expected types.Dict, got {got}",
            r.num, r.generation
        ),
        _ => format!("expected types.Dict, got {got}"),
    };
    Error::Sentinel(Sentinel::ExpectedDict).context(msg)
}

/// `scalarEntryTypeError`.
fn scalar_type_error(key: &[u8], want: &str, o: &Object, obj_nr: i64) -> Error {
    let msg = format!(
        "entry={}: expected {want}, got {}",
        lossy(key),
        type_name(o)
    );
    if obj_nr > 0 {
        Error::Validation(msg)
    } else {
        Error::Corrupt(msg)
    }
}

/// The longest `%v` text [`go_v`] gives; more is cut and ends in `...` (docs/go-differences.md).
const GO_V_CAP: usize = 64 * 1024;

/// Go's `%v` of an object: `<nil>` for null, else its `String()`, cut after [`GO_V_CAP`] bytes.
pub(crate) fn go_v(o: &Object) -> String {
    match o {
        Object::Null => "<nil>".to_string(),
        _ => {
            let mut c = Capped::new(GO_V_CAP);
            o.write_go(&mut c);
            let mut s = lossy(&c.buf);
            if c.cut {
                s.push_str("...");
            }
            s
        }
    }
}

/// `fmt.Errorf("wrong type <%v>", o)` (dereference.go:172, :240, :282, :318, :358).
fn wrong_type(o: &Object) -> Error {
    Error::Corrupt(format!("wrong type <{}>", go_v(o)))
}

/// `entry.Object` as an object. An entry that holds an object stream, an xref stream or a lazy
/// object is not a plain object.
fn entry_object(num: i64, e: &Entry) -> Result<&Object, Error> {
    match &e.object {
        None => Ok(&NULL),
        Some(EntryObject::Obj(o)) => Ok(o),
        Some(_) => Err(Error::Corrupt(format!(
            "obj#{num}: entry is a stream variant or not decoded, not a plain object"
        ))),
    }
}

/// `Text`: the string of a string or hex literal.
///
/// # Errors
/// [`Error::Corrupt`] for any other object, or the UTF-16 errors of the text helpers.
pub fn text(o: &Object) -> Result<String, Error> {
    match o {
        Object::String(s) => string_literal_to_string(s),
        Object::Hex(h) => hex_literal_to_string(h),
        other => Err(Error::Corrupt(format!(
            "corrupt text: {}",
            type_name(other)
        ))),
    }
}

/// `compressedObject`: parses the text of an object of an object stream. A dictionary with a
/// `/Length` would be a stream, which cannot be in an object stream (7.5.7).
///
/// # Errors
/// The parse errors, and [`Error::Corrupt`] for a stream object.
pub fn compressed_object(cx: &mut ParseCtx<'_>, text: &[u8]) -> Result<Object, Error> {
    let mut line = text;
    let o = parse_object_with_policy(cx, &mut line, 0, None)?.object;
    if let Object::Dict(d) = &o {
        let (length, length_ref) = d.length();
        if length.is_some() || length_ref.is_some() {
            return Err(Error::Corrupt(
                "object stream contains stream object".to_string(),
            ));
        }
    }
    Ok(o)
}

impl XRefTable {
    /// `MaxRecursionDepth`: the configured limit, 0 resolved to the default.
    pub fn max_recursion_depth(&self) -> u64 {
        self.conf.limits.recursion_depth()
    }

    /// `CheckRecursionDepth` with the configured limit.
    ///
    /// # Errors
    /// [`Sentinel::MaxRecursionDepth`].
    pub fn check_recursion_depth(&self, name: &str, depth: u64) -> Result<(), Error> {
        check_recursion_depth(name, depth, self.max_recursion_depth())
    }

    // -----------------------------------------------------------------------------------------
    // Lazy object stream objects

    /// Decodes the object stream `stm` in full, once (Go's `sync.Once` of
    /// `objectStreamDecodeFunc`), and compacts its content. The first failure is remembered.
    fn decode_object_stream_once(&mut self, stm: i64, cancel: &AtomicBool) -> Result<(), Error> {
        let Self {
            table,
            budget,
            conf,
            mem,
            ..
        } = self;
        let Some(Entry {
            object: Some(EntryObject::ObjStm(osd)),
            ..
        }) = table.get_mut(&stm)
        else {
            return Err(Error::Corrupt("missing object stream".to_string()));
        };
        let limit = osd.max_decode_bytes;
        if let Err(e) =
            Self::decode_budgeted(&conf.limits, budget, mem, &mut osd.stream, true, cancel)
        {
            osd.stream.content = None;
            if e.is_decode_limit_exceeded() && !e.is_limit_exceeded() {
                return Err(e.context(format!("object stream decode exceeds {limit}-byte limit")));
            }
            return Err(e);
        }
        // `GetData`: drop the spare capacity unless the content is the raw buffer.
        if !osd.stream.preserve_raw_content() {
            let compact = match &osd.stream.content {
                Some(c) if c.len() < c.go_cap() => Some(mem.bytes_from(c, c.len())?),
                _ => None,
            };
            if let Some(b) = compact {
                osd.stream.content = Some(b);
            }
        }
        Ok(())
    }

    /// The decoded content of the object stream `stm`, decoding it on the first use (what
    /// `LazyObjectStreamObject.GetData` does through its `decodeStreamFunc`): the decode is
    /// charged to the cumulative budget once, and its outcome is remembered.
    ///
    /// # Errors
    /// The decode errors of the object stream, the first one again on later calls.
    pub fn lazy_content(&mut self, stm: i64, cancel: &AtomicBool) -> Result<Bytes, Error> {
        let outcome = match self.objstm_state.get(&stm) {
            Some(r) => r.clone(),
            None => {
                let r = self.decode_object_stream_once(stm, cancel);
                if !matches!(r, Err(Error::Cancelled)) {
                    self.objstm_state.insert(stm, r.clone());
                }
                r
            }
        };
        outcome?;
        match self.table.get(&stm) {
            Some(Entry {
                object: Some(EntryObject::ObjStm(osd)),
                ..
            }) => {
                osd.stream.content.clone().ok_or_else(|| {
                    Error::Corrupt("object stream has no decoded content".to_string())
                })
            }
            _ => Err(Error::Corrupt("missing object stream".to_string())),
        }
    }

    /// `LazyObjectStreamObject.DecodedObject` and what `indRefToObject` does with it: decodes
    /// the object stream if needed, parses the object, counts the references in it and stores
    /// it in the entry. If the entry is not a lazy object, nothing happens. On an error the
    /// entry stays lazy.
    ///
    /// # Errors
    /// The decode errors of the object stream, the parse errors, the reference-count walk.
    pub fn decode_lazy_entry(&mut self, obj_nr: i64, cancel: &AtomicBool) -> Result<(), Error> {
        let lazy = match self.table.get(&obj_nr).and_then(|e| e.object.as_ref()) {
            Some(EntryObject::Lazy(l)) => *l,
            _ => return Ok(()),
        };
        let content = self.lazy_content(lazy.stm, cancel)?;
        let text = lazy.slice(&content)?;
        let obj = {
            let mut counter = self.parsed_charge();
            let mut cx = ParseCtx::with_charge(cancel, &mut counter);
            compressed_object(&mut cx, text)?
        };
        process_ref_counts_with_error(self, &obj)?;
        self.grow_reserve(&obj)?;
        if let Some(e) = self.table.get_mut(&obj_nr) {
            e.object = Some(EntryObject::Obj(obj));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Resolving

    /// `indRefToObject`: the object `ir` refers to and the increment of its entry. An undefined
    /// or free target, and an entry that is not loaded, is the null object. With `decode_lazy`
    /// a lazy object is decoded first.
    ///
    /// # Errors
    /// The errors of [`XRefTable::decode_lazy_entry`], and [`Error::Corrupt`] for an entry that
    /// is an object stream or an xref stream.
    pub(crate) fn ind_ref_to_object(
        &mut self,
        ir: Ref,
        decode_lazy: bool,
        cancel: &AtomicBool,
    ) -> Result<(&Object, i64), Error> {
        let lazy = match self.table.get(&ir.num) {
            None => return Ok((&NULL, 0)),
            Some(e) if e.free => return Ok((&NULL, 0)),
            Some(e) => matches!(e.object, Some(EntryObject::Lazy(_))),
        };
        if lazy && decode_lazy {
            self.decode_lazy_entry(ir.num, cancel)?;
        }
        match self.table.get(&ir.num) {
            Some(e) => Ok((entry_object(ir.num, e)?, e.incr)),
            None => Ok((&NULL, 0)),
        }
    }

    /// `Dereference`: `o` itself, or the object it refers to if it is a reference. An undefined
    /// reference is the null object.
    ///
    /// # Errors
    /// As [`XRefTable::ind_ref_to_object`].
    pub fn dereference<'a>(&'a mut self, o: &'a Object) -> Result<&'a Object, Error> {
        self.deref_c(o, &NEVER)
    }

    /// [`XRefTable::dereference`] for the long operations, which pass their cancel flag to a
    /// lazy decode.
    ///
    /// # Errors
    /// As [`XRefTable::dereference`], and [`Error::Cancelled`].
    pub fn deref_c<'a>(
        &'a mut self,
        o: &'a Object,
        cancel: &AtomicBool,
    ) -> Result<&'a Object, Error> {
        match o {
            Object::Ref(ir) => Ok(self.ind_ref_to_object(*ir, true, cancel)?.0),
            _ => Ok(o),
        }
    }

    /// `DereferenceWithIncr`: [`XRefTable::dereference`] and the increment of the entry.
    ///
    /// # Errors
    /// As [`XRefTable::dereference`].
    pub fn dereference_with_incr<'a>(
        &'a mut self,
        o: &'a Object,
    ) -> Result<(&'a Object, i64), Error> {
        match o {
            Object::Ref(ir) => self.ind_ref_to_object(*ir, true, &NEVER),
            _ => Ok((o, 0)),
        }
    }

    /// Resolves `o` with `&self`, without decoding: a reference to a lazy, undefined or free
    /// entry, or to an object stream, is the null object. For after the read path.
    pub fn peek<'a>(&'a self, o: &'a Object) -> &'a Object {
        match o {
            Object::Ref(ir) => match self.table.get(&ir.num) {
                Some(Entry {
                    free: false,
                    object: Some(EntryObject::Obj(r)),
                    ..
                }) => r,
                _ => &NULL,
            },
            _ => o,
        }
    }

    /// `dereferenceEntry` for a value found in a dictionary: the object, whether the key was
    /// found, and the object number if the value is a reference.
    fn dereference_value<'a>(
        &'a mut self,
        key: &[u8],
        o: Option<&'a Object>,
    ) -> Result<(&'a Object, bool, i64), Error> {
        let Some(o) = o else {
            return Ok((&NULL, false, 0));
        };
        if o.is_null() {
            return Ok((&NULL, true, 0));
        }
        let mut obj_nr = 0;
        if let Object::Ref(ir) = o {
            obj_nr = ir.num;
            match self.table.get(&ir.num) {
                Some(e) if !e.free => {}
                _ => {
                    let msg = format!("entry={}: missing indirect target", lossy(key));
                    return Err(if obj_nr > 0 {
                        Error::Validation(msg)
                    } else {
                        Error::Corrupt(msg)
                    });
                }
            }
        }
        match self.dereference(o) {
            Ok(r) => Ok((r, true, obj_nr)),
            Err(e) => Err(e.context(format!("entry={}", lossy(key)))),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Scalars

    /// `DereferenceBoolean`: the boolean `o` is or refers to, `None` for null.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for another type, the version error, the errors of
    /// [`XRefTable::dereference`].
    pub fn dereference_boolean(
        &mut self,
        o: &Object,
        since: Version,
    ) -> Result<Option<bool>, Error> {
        let v = match self.dereference(o)? {
            Object::Null => return Ok(None),
            Object::Bool(b) => *b,
            other => return Err(wrong_type(other)),
        };
        self.validate_version("DereferenceBoolean", since)?;
        Ok(Some(v))
    }

    /// `DereferenceBooleanEntry`: the boolean entry `key` of `d` and whether the key was found.
    ///
    /// # Errors
    /// A type error naming the entry, or the errors of [`XRefTable::dereference`].
    pub fn dereference_boolean_entry(
        &mut self,
        d: &Dict,
        key: &[u8],
    ) -> Result<(Option<bool>, bool), Error> {
        let (o, found, obj_nr) = self.dereference_value(key, d.find(key))?;
        match o {
            Object::Null => Ok((None, found)),
            Object::Bool(b) => Ok((Some(*b), true)),
            other => Err(scalar_type_error(key, "boolean", other, obj_nr)),
        }
    }

    /// `DereferenceInteger`: the integer `o` is or refers to, `None` for null.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for another type, the errors of [`XRefTable::dereference`].
    pub fn dereference_integer(&mut self, o: &Object) -> Result<Option<i64>, Error> {
        match self.dereference(o)? {
            Object::Null => Ok(None),
            Object::Integer(i) => Ok(Some(*i)),
            other => Err(wrong_type(other)),
        }
    }

    /// `DereferenceIntegerEntry`: the integer entry `key` of `d` and whether the key was found.
    ///
    /// # Errors
    /// A type error naming the entry, or the errors of [`XRefTable::dereference`].
    pub fn dereference_integer_entry(
        &mut self,
        d: &Dict,
        key: &[u8],
    ) -> Result<(Option<i64>, bool), Error> {
        let (o, found, obj_nr) = self.dereference_value(key, d.find(key))?;
        match o {
            Object::Null => Ok((None, found)),
            Object::Integer(i) => Ok((Some(*i), true)),
            other => Err(scalar_type_error(key, "integer", other, obj_nr)),
        }
    }

    /// `DereferenceNumber`: the integer or float `o` is or refers to, as a float.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for another type, including null.
    pub fn dereference_number(&mut self, o: &Object) -> Result<f64, Error> {
        match self.dereference(o)? {
            Object::Integer(i) => Ok(*i as f64),
            Object::Float(f) => Ok(*f),
            other => Err(wrong_type(other)),
        }
    }

    /// `DereferenceNumberEntry`: the number entry `key` of `d`, as a float, and whether the key
    /// was found.
    ///
    /// # Errors
    /// A type error naming the entry, or the errors of [`XRefTable::dereference`].
    pub fn dereference_number_entry(
        &mut self,
        d: &Dict,
        key: &[u8],
    ) -> Result<(Option<f64>, bool), Error> {
        let (o, found, obj_nr) = self.dereference_value(key, d.find(key))?;
        match o {
            Object::Null => Ok((None, found)),
            Object::Integer(i) => Ok((Some(*i as f64), true)),
            Object::Float(f) => Ok((Some(*f), true)),
            other => Err(scalar_type_error(key, "number", other, obj_nr)),
        }
    }

    /// `DereferenceName`: the name `o` is or refers to, empty for null. `validate` checks the
    /// value.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for another type or a failed check, the version error, the errors
    /// of [`XRefTable::dereference`].
    pub fn dereference_name(
        &mut self,
        o: &Object,
        since: Version,
        validate: Option<&dyn Fn(&[u8]) -> bool>,
    ) -> Result<Vec<u8>, Error> {
        let n = match self.dereference(o)? {
            Object::Null => return Ok(Vec::new()),
            Object::Name(n) => n.clone(),
            other => return Err(wrong_type(other)),
        };
        self.validate_version("DereferenceName", since)?;
        if let Some(v) = validate
            && !v(n.as_slice())
        {
            return Err(Error::Corrupt(format!("invalid <{}>", lossy(&n))));
        }
        Ok(n)
    }

    /// `DereferenceNameEntry` on a value that was already looked up under `key`.
    pub(crate) fn name_from_value(
        &mut self,
        key: &[u8],
        o: Option<&Object>,
    ) -> Result<(Option<Vec<u8>>, bool), Error> {
        let (o, found, obj_nr) = self.dereference_value(key, o)?;
        match o {
            Object::Null => Ok((None, found)),
            Object::Name(n) => Ok((Some(n.clone()), true)),
            other => Err(scalar_type_error(key, "name", other, obj_nr)),
        }
    }

    /// `DereferenceNameEntry`: the name entry `key` of `d` and whether the key was found.
    ///
    /// # Errors
    /// A type error naming the entry, or the errors of [`XRefTable::dereference`].
    pub fn dereference_name_entry(
        &mut self,
        d: &Dict,
        key: &[u8],
    ) -> Result<(Option<Vec<u8>>, bool), Error> {
        self.name_from_value(key, d.find(key))
    }

    /// `DereferenceStringLiteral`: the string literal `o` is or refers to (the escaped body),
    /// empty for null. `validate` checks the decoded text.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for another type or a failed check, the UTF-16 errors, the version
    /// error, the errors of [`XRefTable::dereference`].
    pub fn dereference_string_literal(
        &mut self,
        o: &Object,
        since: Version,
        validate: Option<&dyn Fn(&str) -> bool>,
    ) -> Result<Vec<u8>, Error> {
        let s = match self.dereference(o)? {
            Object::Null => return Ok(Vec::new()),
            Object::String(s) => s.clone(),
            other => return Err(wrong_type(other)),
        };
        // Ensure UTF16 correctness.
        let text = string_literal_to_string(&s)?;
        self.validate_version("DereferenceStringLiteral", since)?;
        if let Some(v) = validate
            && !v(text.as_str())
        {
            return Err(Error::Corrupt(format!("invalid <{text}>")));
        }
        Ok(s)
    }

    /// `DereferenceStringOrHexLiteral`: the text of the string or hex literal `o` is or refers
    /// to, empty for null.
    ///
    /// # Errors
    /// As [`XRefTable::dereference_string_literal`].
    pub fn dereference_string_or_hex_literal(
        &mut self,
        o: &Object,
        since: Version,
        validate: Option<&dyn Fn(&str) -> bool>,
    ) -> Result<String, Error> {
        let s = match self.dereference(o)? {
            Object::Null => return Ok(String::new()),
            // Ensure UTF16 correctness.
            Object::String(s) => string_literal_to_string(s)?,
            Object::Hex(h) => hex_literal_to_string(h)?,
            _ => {
                return Err(Error::Corrupt(format!("wrong type {}", type_name(o))));
            }
        };
        self.validate_version("DereferenceStringOrHexLiteral", since)?;
        if let Some(v) = validate
            && !v(s.as_str())
        {
            return Err(Error::Corrupt(format!("invalid <{s}>")));
        }
        Ok(s)
    }

    /// `DereferenceStringEntry`: the text of the string or hex entry `key` of `d` and whether
    /// the key was found.
    ///
    /// # Errors
    /// A type error naming the entry, the UTF-16 errors, or the errors of
    /// [`XRefTable::dereference`].
    pub fn dereference_string_entry(
        &mut self,
        d: &Dict,
        key: &[u8],
    ) -> Result<(Option<String>, bool), Error> {
        let (o, found, obj_nr) = self.dereference_value(key, d.find(key))?;
        match o {
            Object::Null => Ok((None, found)),
            Object::String(_) | Object::Hex(_) => match text(o) {
                Ok(s) => Ok((Some(s), true)),
                Err(e) => Err(e.context(format!("entry={}", lossy(key)))),
            },
            other => Err(scalar_type_error(
                key,
                "string or hex literal",
                other,
                obj_nr,
            )),
        }
    }

    /// `DereferenceText`: the text of the string or hex literal `o` is or refers to.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for another type, including null.
    pub fn dereference_text(&mut self, o: &Object) -> Result<String, Error> {
        text(self.dereference(o)?)
    }

    /// `DereferenceStringEntryBytes`: the unescaped bytes of the string or hex entry `key` of
    /// `d`, `None` if there is none.
    ///
    /// # Errors
    /// A type error naming the entry, the hex errors, or the errors of
    /// [`XRefTable::dereference`].
    pub fn dereference_string_entry_bytes(
        &mut self,
        d: &Dict,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, Error> {
        let Some(o) = d.find(key).filter(|o| !o.is_null()) else {
            return Ok(None);
        };
        let obj_nr = match o {
            Object::Ref(r) => r.num,
            _ => 0,
        };
        match self.dereference(o)? {
            Object::String(s) => Ok(Some(unescape(s))),
            Object::Hex(h) => hex_decode(h).map(Some),
            other => Err(scalar_type_error(
                key,
                "string or hex literal",
                other,
                obj_nr,
            )),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Arrays, dictionaries, streams

    /// `DereferenceArray`: the array `o` is or refers to, `None` for null.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for another type, the errors of [`XRefTable::dereference`].
    pub fn dereference_array<'a>(&'a mut self, o: &'a Object) -> Result<Option<&'a Array>, Error> {
        match self.dereference(o)? {
            Object::Null => Ok(None),
            Object::Array(a) => Ok(Some(a)),
            other => Err(Error::Corrupt(format!(
                "wrong type {} <{}>",
                type_name(other),
                go_v(other)
            ))),
        }
    }

    /// `DereferenceDict` for a reference: the dictionary it refers to, `None` for null.
    ///
    /// # Errors
    /// [`Sentinel::ExpectedDict`] for another type, the errors of [`XRefTable::dereference`].
    pub fn dereference_dict_ref(&mut self, ir: Ref) -> Result<Option<&Dict>, Error> {
        let (r, _) = self.ind_ref_to_object(ir, true, &NEVER)?;
        match r {
            Object::Null => Ok(None),
            Object::Dict(d) => Ok(Some(d)),
            other => Err(dict_type_error(&Object::Ref(ir), other)),
        }
    }

    /// `DereferenceDict`: the dictionary `o` is or refers to, `None` for null.
    ///
    /// # Errors
    /// [`Sentinel::ExpectedDict`] for another type, the errors of [`XRefTable::dereference`].
    pub fn dereference_dict<'a>(&'a mut self, o: &'a Object) -> Result<Option<&'a Dict>, Error> {
        match o {
            Object::Ref(r) => self.dereference_dict_ref(*r),
            Object::Null => Ok(None),
            Object::Dict(d) => Ok(Some(d)),
            other => Err(dict_type_error(o, other)),
        }
    }

    /// `DereferenceDict` to change the dictionary a reference refers to (Go returns the stored
    /// map, so writes through it persist).
    ///
    /// # Errors
    /// [`Sentinel::ExpectedDict`] for another type, the errors of [`XRefTable::dereference`].
    pub fn dereference_dict_mut(&mut self, ir: Ref) -> Result<Option<&mut Dict>, Error> {
        // Decode a lazy object, and find out whether the target is a dictionary.
        let (r, _) = self.ind_ref_to_object(ir, true, &NEVER)?;
        match r {
            Object::Null => return Ok(None),
            Object::Dict(_) => {}
            other => return Err(dict_type_error(&Object::Ref(ir), other)),
        }
        match self.table.get_mut(&ir.num) {
            Some(Entry {
                object: Some(EntryObject::Obj(Object::Dict(d))),
                ..
            }) => Ok(Some(d)),
            _ => Ok(None),
        }
    }

    /// `DereferenceDictWithIncr`: [`XRefTable::dereference_dict`] and the increment of the
    /// entry (the number of the written increment the object is part of).
    ///
    /// # Errors
    /// As [`XRefTable::dereference_dict`].
    pub fn dereference_dict_with_incr<'a>(
        &'a mut self,
        o: &'a Object,
    ) -> Result<(Option<&'a Dict>, i64), Error> {
        let (r, incr) = self.dereference_with_incr(o)?;
        match r {
            Object::Null => Ok((None, 0)),
            Object::Dict(d) => Ok((Some(d), incr)),
            other => Err(dict_type_error(o, other)),
        }
    }

    /// `DereferenceFontDict`: the font dictionary `ir` refers to. Relaxed validation does not
    /// check its `/Type`.
    ///
    /// # Errors
    /// As [`XRefTable::dereference_dict_ref`].
    pub fn dereference_font_dict(&mut self, ir: Ref) -> Result<Option<&Dict>, Error> {
        self.dereference_dict_ref(ir)
    }

    /// `dereferencePageNodeDictType`: the `/Type` name of the page node `ir`.
    fn page_node_type(&mut self, ir: Ref) -> Result<Vec<u8>, Error> {
        let Some(d) = self.dereference_dict_ref(ir)? else {
            return Err(Error::Corrupt("missing page node dict".to_string()));
        };
        let t = d.find(b"Type").cloned();
        let (name, _) = self.name_from_value(b"Type", t.as_ref())?;
        name.ok_or_else(|| Error::Corrupt("missing dict type".to_string()))
    }

    /// `DereferencePageNodeDict`: the `Pages` or `Page` node `ir` refers to.
    ///
    /// # Errors
    /// [`Error::Corrupt`] for a missing node, a missing or another `/Type`.
    pub fn dereference_page_node_dict(&mut self, ir: Ref) -> Result<&Dict, Error> {
        let ty = self.page_node_type(ir)?;
        if !matches!(ty.as_slice(), b"Pages" | b"Page") {
            return Err(Error::Corrupt(format!("unexpected Type: {}", lossy(&ty))));
        }
        self.dereference_dict_ref(ir)?
            .ok_or_else(|| Error::Corrupt("missing page node dict".to_string()))
    }

    /// `DereferenceDictEntry`: the dereferenced entry `key` of `d`.
    ///
    /// # Errors
    /// [`Error::Corrupt`] if the entry is missing or null, or the errors of
    /// [`XRefTable::dereference`].
    pub fn dereference_dict_entry<'a>(
        &'a mut self,
        d: &'a Dict,
        key: &[u8],
    ) -> Result<&'a Object, Error> {
        let Some(o) = d.find(key).filter(|o| !o.is_null()) else {
            return Err(Error::Corrupt(format!("dict entry={} missing", lossy(key))));
        };
        self.dereference(o)
    }

    /// `DereferenceXObjectDict`: the image or form XObject `ir` refers to, as
    /// [`XRefTable::dereference_stream_dict`] returns a stream.
    ///
    /// # Errors
    /// The errors of `dereference_stream_dict` and `dereference_name_entry`, and
    /// [`Error::Corrupt`] for a `/Subtype` that is neither `Image` nor `Form`.
    pub fn dereference_xobject_dict(&mut self, ir: Ref) -> Result<Option<StreamDict>, Error> {
        let (sd, _) = self.dereference_stream_dict(&Object::Ref(ir))?;
        let Some(sd) = sd else {
            return Ok(None);
        };
        let (sub, _) = self
            .dereference_name_entry(&sd.dict, b"Subtype")
            .map_err(|e| {
                e.context(format!(
                    "dereferenceXObjectDict: stream dict Subtype {} {} R",
                    ir.num, ir.generation
                ))
            })?;
        match sub {
            // Relaxed validation accepts a missing or empty `/Subtype`.
            None => Ok(Some(sd)),
            Some(s) if s.is_empty() => Ok(Some(sd)),
            Some(s) if s == b"Image" || s == b"Form" => Ok(Some(sd)),
            Some(s) => Err(Error::Corrupt(format!(
                "dereferenceXObjectDict: unexpected stream dict Subtype {}",
                lossy(&s)
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Limits};
    use crate::object::{LazyRef, ObjectStreamDict, PdfFilter};

    fn table() -> XRefTable {
        XRefTable::new(Config::default())
    }

    fn r(n: i64) -> Ref {
        Ref {
            num: n,
            generation: 0,
        }
    }

    fn corrupt(msg: &str) -> Error {
        Error::Corrupt(msg.to_string())
    }

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    /// A table whose object stream 10 holds `content`, already decoded.
    fn objstm_table(content: &[u8]) -> XRefTable {
        let mut x = table();
        let mut osd = ObjectStreamDict::new();
        osd.stream.filter_pipeline.clear();
        osd.stream.content = Some(x.mem().bytes_from(content, content.len()).unwrap());
        x.table.insert(
            10,
            Entry {
                generation: Some(0),
                object: Some(EntryObject::ObjStm(Box::new(osd))),
                ..Entry::default()
            },
        );
        x
    }

    fn lazy_entry(stm: i64, start: i64, end: i64) -> Entry {
        Entry {
            generation: Some(0),
            compressed: false,
            object: Some(EntryObject::Lazy(LazyRef { stm, start, end })),
            ..Entry::default()
        }
    }

    // Port of TestUnresolvedReferenceRemainsNull.
    #[test]
    fn unresolved_reference_remains_null() {
        let mut x = table();
        x.root = Some(r(7));
        assert!(x.catalog().is_err());
        let (o, incr) = x.ind_ref_to_object(r(7), true, &NEVER).unwrap();
        assert!(o.is_null() && incr == 0);
        let o = Object::Ref(r(7));
        assert!(x.dereference(&o).unwrap().is_null());
        // A free target is null as well, and a direct object is itself.
        x.table.insert(
            7,
            Entry {
                free: true,
                ..Entry::default()
            },
        );
        assert!(x.dereference(&o).unwrap().is_null());
        let i = Object::Integer(3);
        assert!(matches!(x.dereference(&i), Ok(Object::Integer(3))));
    }

    #[test]
    fn dereference_follows_one_reference() {
        let mut x = table();
        x.table.insert(2, Entry::gen0(Object::Ref(r(3))));
        x.table.insert(3, Entry::gen0(Object::Integer(9)));
        let o = Object::Ref(r(2));
        // One level, as Go: the result is the reference stored in the entry.
        assert!(matches!(x.dereference(&o), Ok(Object::Ref(_))));
        assert!(matches!(x.peek(&o), Object::Ref(_)));
        assert!(matches!(x.peek(&Object::Ref(r(3))), Object::Integer(9)));
        assert!(x.peek(&Object::Ref(r(4))).is_null());
        let mut e = Entry::gen0(Object::Integer(1));
        e.incr = 4;
        x.table.insert(4, e);
        let o4 = Object::Ref(r(4));
        let (obj, incr) = x.dereference_with_incr(&o4).unwrap();
        assert!(matches!(obj, Object::Integer(1)) && incr == 4);
        let (obj, incr) = x.dereference_with_incr(&Object::Null).unwrap();
        assert!(obj.is_null() && incr == 0);
    }

    #[test]
    fn stream_variants_are_not_plain_objects() {
        let mut x = objstm_table(b"1");
        let o = Object::Ref(r(10));
        assert!(matches!(x.dereference(&o), Err(Error::Corrupt(_))));
        assert!(x.peek(&o).is_null());
        assert!(matches!(
            x.find(10).unwrap().object,
            Some(EntryObject::ObjStm(_))
        ));
    }

    #[test]
    fn lazy_objects_decode_on_first_use() {
        let a = b"<</Type /Catalog /Pages 3 0 R>>";
        let b = b"[1 2 3 0 R]";
        let mut content = a.to_vec();
        content.push(b' ');
        content.extend_from_slice(b);
        let mut x = objstm_table(&content);
        x.table.insert(1, lazy_entry(10, 0, a.len() as i64));
        x.table.insert(2, lazy_entry(10, a.len() as i64 + 1, -1));
        x.table.insert(3, Entry::gen0(Object::Integer(0)));

        let o1 = Object::Ref(r(1));
        let d = x.dereference_dict(&o1).unwrap().unwrap();
        assert_eq!(d.name_entry(b"Type"), Some(b"Catalog".as_slice()));
        // The entry holds the object now, and the reference in it is counted.
        assert!(matches!(
            x.find(1).unwrap().object,
            Some(EntryObject::Obj(Object::Dict(_)))
        ));
        assert_eq!(x.find(3).unwrap().ref_count, 1);
        assert_eq!(x.objstm_state.get(&10), Some(&Ok(())));

        let o2 = Object::Ref(r(2));
        let arr = x.dereference_array(&o2).unwrap().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(x.find(3).unwrap().ref_count, 2);
        // The stream is not charged to the document's budget twice.
        assert_eq!(x.budget().obj_stm, 0);
    }

    #[test]
    fn lazy_catalog_resolves_through_root() {
        let a = b"<</Pages 4 0 R /Version /1.6>>";
        let mut x = objstm_table(a);
        x.table.insert(1, lazy_entry(10, 0, -1));
        x.root = Some(r(1));
        assert_eq!(x.pages(), Ok(r(4)));
        assert_eq!(x.parse_root_version(), Ok(Some(b"1.6".to_vec())));
    }

    #[test]
    fn lazy_object_that_is_a_stream_is_refused() {
        let mut x = objstm_table(b"<</Length 5>>");
        x.table.insert(1, lazy_entry(10, 0, -1));
        let o = Object::Ref(r(1));
        assert_eq!(
            x.dereference(&o).map(|_| ()),
            Err(corrupt("object stream contains stream object"))
        );
        // The entry stays lazy.
        assert!(matches!(
            x.find(1).unwrap().object,
            Some(EntryObject::Lazy(_))
        ));
    }

    // Port of TestCatalogAccessPreservesDecodeError: the decode error is returned as it is.
    #[test]
    fn catalog_access_preserves_the_decode_error() {
        let mut x = table();
        x.root = Some(r(1));
        x.table.insert(1, lazy_entry(99, 0, -1));
        let want = corrupt("missing object stream");
        assert_eq!(x.catalog().map(|_| ()), Err(want.clone()));
        assert_eq!(x.pages().map(|_| ()), Err(want.clone()));
        assert_eq!(x.parse_root_version().map(|_| ()), Err(want.clone()));
        // The first outcome is remembered per object stream.
        assert_eq!(x.objstm_state.get(&99), Some(&Err(want)));
    }

    #[test]
    fn lazy_object_outside_its_stream_is_an_error() {
        let mut x = objstm_table(b"[1]");
        x.table.insert(1, lazy_entry(10, 2, 1));
        x.table.insert(2, lazy_entry(10, 0, 99));
        for n in [1, 2] {
            let o = Object::Ref(r(n));
            assert!(matches!(x.dereference(&o), Err(Error::Corrupt(_))), "{n}");
        }
    }

    /// An ASCIIHex stream of "1234567890", whose decoded capacity is its length.
    fn hex_stream(x: &XRefTable) -> StreamDict {
        StreamDict {
            raw: Some(x.mem().bytes_from(b"31323334353637383930>", 21).unwrap()),
            filter_pipeline: vec![PdfFilter {
                name: b"ASCIIHexDecode".to_vec(),
                decode_parms: None,
            }],
            ..StreamDict::default()
        }
    }

    // The `bt` boundary of the parity corpus: `GetData` of a lazy object decodes its object
    // stream through `DecodeObjectStream`, so a total budget the plain streams used up refuses
    // it, and the refusal is remembered for the later reads (`objectStreamDecodeFunc`).
    #[test]
    fn lazy_object_stream_decode_is_charged_to_the_total_budget() {
        let cancel = AtomicBool::new(false);
        for total in [19, 20] {
            let mut x = XRefTable::new(Config {
                limits: Limits {
                    max_total_decode_bytes: total,
                    ..Limits::default()
                },
                ..Config::default()
            });
            let mut osd = ObjectStreamDict::new();
            osd.stream = hex_stream(&x);
            x.table.insert(
                10,
                Entry {
                    generation: Some(0),
                    object: Some(EntryObject::ObjStm(Box::new(osd))),
                    ..Entry::default()
                },
            );
            x.table.insert(1, lazy_entry(10, 0, -1));
            let mut sd = hex_stream(&x);
            assert_eq!(x.decode_stream(&mut sd, &cancel), Ok(()));
            assert_eq!(x.budget().used, 10);

            let got = x.lazy_content(10, &cancel);
            if total == 20 {
                assert_eq!(&got.unwrap()[..], b"1234567890");
                assert_eq!(x.budget().used, 20);
                continue;
            }
            assert_eq!(got.unwrap_err().class(), "limit/total-decode");
            assert_eq!(x.budget().used, 10);
            let again = x.lazy_content(10, &cancel).unwrap_err();
            assert_eq!(again.class(), "limit/total-decode");
            let o = Object::Ref(r(1));
            let err = x.dereference(&o).unwrap_err();
            assert_eq!(err.class(), "limit/total-decode");
            assert_eq!(x.budget().used, 10);
        }
    }

    #[test]
    fn lazy_decode_charges_parsed_bytes() {
        let mut x = objstm_table(b"<</A [1 2]>>");
        x.conf.limits.max_parsed_bytes = 10_000;
        x.table.insert(1, lazy_entry(10, 0, -1));
        let o = Object::Ref(r(1));
        assert!(x.dereference(&o).is_ok());
        assert!(x.budget().parsed > 0);

        let mut x = objstm_table(b"<</A [1 2]>>");
        x.conf.limits.max_parsed_bytes = 10;
        x.table.insert(1, lazy_entry(10, 0, -1));
        let err = x.dereference(&o).map(|_| ()).unwrap_err();
        assert!(err.is_limit_exceeded());
        assert_eq!(err.class(), "limit/parsed");
    }

    // Reference counts.

    #[test]
    fn process_ref_counts_counts_nested_references() {
        let mut x = table();
        for n in [2, 3] {
            x.table.insert(n, Entry::gen0(Object::Null));
        }
        let inner = dict(&[("c", Object::Ref(r(3)))]);
        let d = dict(&[
            ("a", Object::Ref(r(2))),
            (
                "b",
                Object::Array(vec![Object::Ref(r(2)), Object::Dict(inner)]),
            ),
            ("d", Object::Ref(r(99))),
        ]);
        assert_eq!(
            process_ref_counts_with_error(&mut x, &Object::Dict(d)),
            Ok(())
        );
        assert_eq!(x.find(2).unwrap().ref_count, 2);
        assert_eq!(x.find(3).unwrap().ref_count, 1);
        // A stream counts the references of its dictionary.
        let sd = crate::object::StreamDict {
            dict: dict(&[("x", Object::Ref(r(3)))]),
            ..crate::object::StreamDict::default()
        };
        process_ref_counts(&mut x, &Object::Stream(Box::new(sd)));
        assert_eq!(x.find(3).unwrap().ref_count, 2);
        // Scalars have nothing to count.
        assert_eq!(
            process_ref_counts_with_error(&mut x, &Object::Integer(1)),
            Ok(())
        );
    }

    #[test]
    fn ref_count_walk_is_bounded_by_the_recursion_limit() {
        let mut x = table();
        x.conf.limits.max_recursion_depth = 1;
        let deep = Object::Dict(dict(&[(
            "a",
            Object::Dict(dict(&[("b", Object::Dict(Dict::default()))])),
        )]));
        let err = process_ref_counts_with_error(&mut x, &deep).unwrap_err();
        assert!(err.is(Sentinel::MaxRecursionDepth));
        assert!(
            err.to_string().contains("reference count traversal"),
            "{err}"
        );
        let shallow = Object::Dict(dict(&[("a", Object::Dict(Dict::default()))]));
        assert_eq!(process_ref_counts_with_error(&mut x, &shallow), Ok(()));
    }

    #[test]
    fn check_recursion_depth_uses_the_default_for_zero() {
        assert_eq!(check_recursion_depth("x", 100, 0), Ok(()));
        assert!(check_recursion_depth("x", 101, 0).is_err());
        assert_eq!(check_recursion_depth("x", 5, 5), Ok(()));
        assert!(check_recursion_depth("x", 6, 5).is_err());
        assert_eq!(check_recursion_depth("x", 256, u64::MAX), Ok(()));
        assert!(check_recursion_depth("x", 257, u64::MAX).is_err());
        let mut x = table();
        x.conf.limits.max_recursion_depth = 0;
        assert_eq!(x.max_recursion_depth(), 100);
        assert!(
            x.check_recursion_depth("y", 101)
                .unwrap_err()
                .is(Sentinel::MaxRecursionDepth)
        );
    }

    // Typed dereferencing.

    fn typed_table() -> (XRefTable, Dict) {
        let mut x = table();
        x.table.insert(4, Entry::gen0(Object::Integer(7)));
        x.table.insert(5, Entry::gen0(Object::Float(2.5)));
        x.table.insert(6, Entry::gen0(Object::name("N")));
        x.table.insert(7, Entry::gen0(Object::Bool(true)));
        x.table.insert(
            8,
            Entry {
                free: true,
                ..Entry::default()
            },
        );
        let d = dict(&[
            ("I", Object::Integer(5)),
            ("IR", Object::Ref(r(4))),
            ("F", Object::Ref(r(5))),
            ("N", Object::Ref(r(6))),
            ("B", Object::Ref(r(7))),
            ("S", Object::string("abc")),
            ("Gone", Object::Ref(r(8))),
            ("Undef", Object::Ref(r(40))),
            ("Null", Object::Null),
            ("Name", Object::name("x")),
        ]);
        (x, d)
    }

    #[test]
    fn typed_entries() {
        let (mut x, d) = typed_table();
        assert_eq!(x.dereference_integer_entry(&d, b"I"), Ok((Some(5), true)));
        assert_eq!(x.dereference_integer_entry(&d, b"IR"), Ok((Some(7), true)));
        assert_eq!(x.dereference_integer_entry(&d, b"Nope"), Ok((None, false)));
        assert_eq!(x.dereference_integer_entry(&d, b"Null"), Ok((None, true)));
        assert_eq!(x.dereference_number_entry(&d, b"I"), Ok((Some(5.0), true)));
        assert_eq!(x.dereference_number_entry(&d, b"F"), Ok((Some(2.5), true)));
        assert_eq!(
            x.dereference_name_entry(&d, b"N"),
            Ok((Some(b"N".to_vec()), true))
        );
        assert_eq!(
            x.dereference_boolean_entry(&d, b"B"),
            Ok((Some(true), true))
        );
        assert_eq!(
            x.dereference_string_entry(&d, b"S"),
            Ok((Some("abc".to_string()), true))
        );
        assert_eq!(
            x.dereference_string_entry_bytes(&d, b"S"),
            Ok(Some(b"abc".to_vec()))
        );
        assert_eq!(x.dereference_string_entry_bytes(&d, b"Nope"), Ok(None));
    }

    #[test]
    fn typed_entry_errors() {
        let (mut x, d) = typed_table();
        // A reference to a free or undefined object is an error naming the entry.
        for key in [b"Gone".as_slice(), b"Undef"] {
            let err = x.dereference_integer_entry(&d, key).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "pdf: validation: entry={}: missing indirect target",
                    String::from_utf8_lossy(key)
                )
            );
        }
        assert_eq!(
            x.dereference_integer_entry(&d, b"Name"),
            Err(corrupt("entry=Name: expected integer, got types.Name"))
        );
        // With an object number the type error is a validation error.
        assert_eq!(
            x.dereference_boolean_entry(&d, b"IR"),
            Err(Error::Validation(
                "entry=IR: expected boolean, got types.Integer".to_string()
            ))
        );
        assert!(x.dereference_name_entry(&d, b"I").is_err());
        assert!(x.dereference_number_entry(&d, b"Name").is_err());
        assert!(x.dereference_string_entry(&d, b"I").is_err());
        assert!(x.dereference_string_entry_bytes(&d, b"I").is_err());
    }

    #[test]
    fn go_v_is_cut_at_the_cap() {
        // Wide, not deep: 40 000 levels would overflow a test thread's stack.
        let v = Object::Array(vec![Object::Integer(7); 40_000]);
        let s = go_v(&v);
        assert_eq!(s.len(), GO_V_CAP + 3);
        assert!(s.starts_with("[7 7 "));
        assert!(s.ends_with("..."));
        assert_eq!(go_v(&Object::Integer(5)), "5");
    }

    #[test]
    fn scalar_dereference() {
        let (mut x, _) = typed_table();
        x.header_version = Some(Version::V14);
        let o = |n: i64| Object::Ref(r(n));
        assert_eq!(x.dereference_integer(&o(4)), Ok(Some(7)));
        assert_eq!(x.dereference_integer(&Object::Null), Ok(None));
        assert_eq!(x.dereference_integer(&Object::Ref(r(40))), Ok(None));
        assert_eq!(x.dereference_integer(&o(6)), Err(corrupt("wrong type <N>")));
        assert_eq!(
            x.dereference_string_or_hex_literal(&o(4), Version::V10, None),
            Err(corrupt("wrong type types.IndirectRef"))
        );
        assert_eq!(x.dereference_number(&o(5)), Ok(2.5));
        assert_eq!(x.dereference_number(&Object::Integer(3)), Ok(3.0));
        assert_eq!(
            x.dereference_number(&Object::Null),
            Err(corrupt("wrong type <<nil>>"))
        );
        assert_eq!(x.dereference_boolean(&o(7), Version::V10), Ok(Some(true)));
        assert_eq!(x.dereference_boolean(&Object::Null, Version::V10), Ok(None));
        assert!(x.dereference_boolean(&o(7), Version::V15).is_err());
        assert!(x.dereference_boolean(&o(4), Version::V10).is_err());

        assert_eq!(
            x.dereference_name(&o(6), Version::V10, None),
            Ok(b"N".to_vec())
        );
        assert_eq!(
            x.dereference_name(&Object::Null, Version::V10, None),
            Ok(Vec::new())
        );
        let only_m = |n: &[u8]| n == b"M";
        assert_eq!(
            x.dereference_name(&o(6), Version::V10, Some(&only_m)),
            Err(corrupt("invalid <N>"))
        );
        assert!(x.dereference_name(&o(6), Version::V20, None).is_err());

        assert_eq!(
            x.dereference_string_literal(&Object::string("a\\(b"), Version::V10, None),
            Ok(b"a\\(b".to_vec())
        );
        let no = |_: &str| false;
        assert_eq!(
            x.dereference_string_literal(&Object::string("hi"), Version::V10, Some(&no)),
            Err(corrupt("invalid <hi>"))
        );
        assert_eq!(
            x.dereference_string_or_hex_literal(&Object::string("hi"), Version::V10, None),
            Ok("hi".to_string())
        );
        assert_eq!(
            x.dereference_string_or_hex_literal(&Object::Hex(b"4869".to_vec()), Version::V10, None),
            Ok("Hi".to_string())
        );
        assert_eq!(
            x.dereference_string_or_hex_literal(&Object::Null, Version::V10, None),
            Ok(String::new())
        );
        assert!(
            x.dereference_string_or_hex_literal(&Object::Integer(1), Version::V10, None)
                .is_err()
        );
        assert_eq!(
            x.dereference_text(&Object::string("t")),
            Ok("t".to_string())
        );
        assert_eq!(text(&Object::Hex(b"4869".to_vec())), Ok("Hi".to_string()));
        assert!(text(&Object::Null).is_err());
    }

    #[test]
    fn arrays_and_dicts() {
        let mut x = table();
        x.table.insert(
            2,
            Entry::gen0(Object::Dict(dict(&[("K", Object::Integer(1))]))),
        );
        x.table
            .insert(3, Entry::gen0(Object::Array(vec![Object::Integer(1)])));
        x.table.insert(4, Entry::gen0(Object::Integer(1)));
        let (d, a, i) = (Object::Ref(r(2)), Object::Ref(r(3)), Object::Ref(r(4)));
        assert_eq!(
            x.dereference_dict(&d).unwrap().unwrap().int_entry(b"K"),
            Some(1)
        );
        assert!(matches!(x.dereference_dict(&Object::Null), Ok(None)));
        assert!(matches!(x.dereference_dict(&Object::Ref(r(40))), Ok(None)));
        let direct = Object::Dict(Dict::default());
        assert!(x.dereference_dict(&direct).unwrap().unwrap().is_empty());
        let err = x.dereference_dict(&i).unwrap_err();
        assert!(err.is(Sentinel::ExpectedDict));
        assert_eq!(
            err.to_string(),
            "obj#4 gen#0: expected types.Dict, got types.Integer"
        );
        let err = x.dereference_dict(&Object::Integer(1)).unwrap_err();
        assert!(err.is(Sentinel::ExpectedDict));
        assert_eq!(err.to_string(), "expected types.Dict, got types.Integer");
        assert_eq!(
            x.dereference_array(&i).unwrap_err(),
            corrupt("wrong type types.Integer <1>")
        );
        assert!(x.dereference_dict(&a).is_err());

        assert_eq!(x.dereference_array(&a).unwrap().unwrap().len(), 1);
        assert!(matches!(x.dereference_array(&Object::Null), Ok(None)));
        assert!(x.dereference_array(&d).is_err());

        let (dd, incr) = x.dereference_dict_with_incr(&d).unwrap();
        assert!(dd.is_some() && incr == 0);

        // Writes through the mutable accessor persist.
        x.dereference_dict_mut(r(2))
            .unwrap()
            .unwrap()
            .insert_int(b"N", 5);
        assert_eq!(
            x.dereference_dict(&d).unwrap().unwrap().int_entry(b"N"),
            Some(5)
        );
        assert!(matches!(x.dereference_dict_mut(r(40)), Ok(None)));
        assert!(
            x.dereference_dict_mut(r(4))
                .unwrap_err()
                .is(Sentinel::ExpectedDict)
        );

        let holder = dict(&[("E", Object::Ref(r(4))), ("Nul", Object::Null)]);
        assert!(matches!(
            x.dereference_dict_entry(&holder, b"E"),
            Ok(Object::Integer(1))
        ));
        assert!(x.dereference_dict_entry(&holder, b"Nul").is_err());
        assert!(x.dereference_dict_entry(&holder, b"Missing").is_err());
    }

    #[test]
    fn font_and_page_node_dicts() {
        let mut x = table();
        let node = |t: Option<&str>| match t {
            Some(t) => Object::Dict(dict(&[("Type", Object::name(t))])),
            None => Object::Dict(Dict::default()),
        };
        x.table.insert(2, Entry::gen0(node(Some("Pages"))));
        x.table.insert(3, Entry::gen0(node(Some("Page"))));
        x.table.insert(4, Entry::gen0(node(Some("Font"))));
        x.table.insert(5, Entry::gen0(node(None)));
        assert!(x.dereference_page_node_dict(r(2)).is_ok());
        assert!(x.dereference_page_node_dict(r(3)).is_ok());
        assert_eq!(
            x.dereference_page_node_dict(r(4)).map(|_| ()),
            Err(corrupt("unexpected Type: Font"))
        );
        assert_eq!(
            x.dereference_page_node_dict(r(5)).map(|_| ()),
            Err(corrupt("missing dict type"))
        );
        assert_eq!(
            x.dereference_page_node_dict(r(40)).map(|_| ()),
            Err(corrupt("missing page node dict"))
        );
        // Relaxed validation does not check the font's /Type.
        assert!(x.dereference_font_dict(r(5)).unwrap().is_some());
        assert!(matches!(x.dereference_font_dict(r(40)), Ok(None)));
    }

    #[test]
    fn xobject_dicts() {
        let mut x = table();
        let xo = |sub: Option<&str>| {
            let mut sd = StreamDict::default();
            if let Some(s) = sub {
                sd.insert_name(b"Subtype", s.as_bytes());
            }
            Object::Stream(Box::new(sd))
        };
        x.table.insert(2, Entry::gen0(xo(Some("Image"))));
        x.table.insert(3, Entry::gen0(xo(Some("Form"))));
        x.table.insert(4, Entry::gen0(xo(Some("PS"))));
        x.table.insert(5, Entry::gen0(xo(None)));
        assert!(x.dereference_xobject_dict(r(2)).unwrap().is_some());
        assert!(x.dereference_xobject_dict(r(3)).unwrap().is_some());
        assert_eq!(
            x.dereference_xobject_dict(r(4)).map(|_| ()),
            Err(corrupt(
                "dereferenceXObjectDict: unexpected stream dict Subtype PS"
            ))
        );
        // Relaxed validation accepts a missing /Subtype.
        assert!(x.dereference_xobject_dict(r(5)).unwrap().is_some());
        assert!(x.dereference_xobject_dict(r(40)).unwrap().is_none());
        assert!(x.find(2).unwrap().valid);
    }

    #[test]
    fn compressed_object_parses_one_object() {
        let cancel = AtomicBool::new(false);
        let mut cx = ParseCtx::new(&cancel);
        assert!(matches!(
            compressed_object(&mut cx, b"<</A 1>>"),
            Ok(Object::Dict(_))
        ));
        assert!(matches!(
            compressed_object(&mut cx, b" [1 2] "),
            Ok(Object::Array(_))
        ));
        assert!(matches!(
            compressed_object(&mut cx, b"42"),
            Ok(Object::Integer(42))
        ));
        assert!(compressed_object(&mut cx, b"<</Length 5 0 R>>").is_err());
        assert!(compressed_object(&mut cx, b"").is_err());
    }
}
