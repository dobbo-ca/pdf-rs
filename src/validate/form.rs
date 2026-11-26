//! Interactive forms (Go: validate/form.go, form_hierarchy.go).
//!
//! Relaxed validation only. Go changes the stored dictionaries through the maps it is given; here
//! a validator works on a copy and stores it back (`store_dict`):
//!
//! * a form field dictionary is stored back when the field is done, whatever the outcome;
//! * the `AcroForm` dictionary is stored back by [`sync_acro_form`], to the object it is (a
//!   reference) or to the catalog copy it is in (a direct dictionary). `validate_form_context` does
//!   it when it is done and `validateRootObject` does it again after
//!   [`validate_form_fields_against_page_annotations`], which changes `XRefTable::form` only;
//! * an appearance dictionary is stored back if it is a reference, and returned if it is a direct
//!   dictionary that a validator changed.
//!
//! The functions of other packages that this file calls (the signatures WP 18 assumed, following
//! the stub conventions):
//!
//! * `action::validate_additional_actions_with_owner(v, d, dict_name, entry_name, required, since,
//!   source)`;
//! * `action::with_form_action_pages(v)`, Go's `withFormActionPages`, which starts the page index
//!   of one form validation and returns the one it replaces (restored when the form is done);
//! * `action::validate_form_actions_on_page(v, obj_nr, d, terminal, validate)`;
//! * `annotation::validate_annotation_dict(v, d, owner_obj_nr)` and
//!   `xobject::validate_xobject_stream_dict(v, o)`.
//!
//! Differences from Go:
//!
//! * The spec violations that Go collects and shows (`specViolations`) are not kept.
//! * `validateDA`, the strict form of [`validate_da_relaxed`], is not ported.
//! * [`validate_da_relaxed`] parses numbers with Rust's `str::parse`: Go's `ParseFloat` also
//!   accepts hexadecimal floats and underscores, which a default appearance string does not use.
//!   A number that overflows `float32` (`rg`, `g`) or `float64` (`Tf`) is rejected, as in Go.
//! * A cancelled validation is not swallowed where relaxed validation deletes a corrupt
//!   appearance entry.

use super::Validator;
use super::action::{
    validate_additional_actions_with_owner, validate_form_actions_on_page, with_form_action_pages,
};
use super::annotation::validate_annotation_dict;
use super::indirect_object_increment;
use super::object::{
    OPTIONAL, REQUIRED, validate_array_pairs, validate_boolean_entry, validate_dict_entry,
    validate_entry, validate_ind_ref_entry, validate_integer_entry, validate_name_entry,
    validate_rectangle_entry, validate_string_entry,
};
use super::page::validate_resource_dict;
use super::state::{SigType, Signature};
use super::xobject::{validate_xobject_stream_dict, validate_xobject_stream_dict_contents};
use crate::annotation::{AnnotationType, PgAnnots, rect_for_array, rect_short_string};
use crate::error::{Error, Sentinel};
use crate::model::deref::type_name;
use crate::model::{Entry, EntryObject, Version, XRefTable};
use crate::object::text::string_or_hex_literal;
use crate::object::{Array, Dict, Object, Rect, Ref};
use std::collections::{BTreeMap, BTreeSet};

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Stores `d` as the dictionary of the object `r` (Go changes the stored map in place).
fn store_dict(x: &mut XRefTable, r: Ref, d: Dict) {
    if let Some(Entry {
        object: Some(EntryObject::Obj(stored @ Object::Dict(_))),
        ..
    }) = x.table.get_mut(&r.num)
    {
        *stored = Object::Dict(d);
    }
}

/// `WrapRecursionError`: the context, unless the recursion limit was exceeded.
fn wrap_recursion_error(context: String, e: Error) -> Error {
    if e.is(Sentinel::MaxRecursionDepth) {
        e
    } else {
        e.context(context)
    }
}

/// An array element that must be a reference.
fn expect_ref(o: &Object, context: &str) -> Result<Ref, Error> {
    match o {
        Object::Ref(r) => Ok(*r),
        other => Err(Error::Validation(format!(
            "{context}: expected indirect reference, got {}",
            type_name(other)
        ))),
    }
}

// ---------------------------------------------------------------------------------------------
// Appearance dictionaries

/// `validateAppearanceSubDict`: a dictionary of appearance streams, by appearance state. Relaxed
/// validation skips an empty dictionary.
fn validate_appearance_sub_dict(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    for (key, o) in d.iter() {
        v.check()?;
        if let Object::Dict(d1) = o
            && d1.is_empty()
        {
            continue;
        }
        validate_xobject_stream_dict(v, o)
            .map_err(|e| e.context(format!("appearance subdict entry {}", lossy(key))))?;
    }
    Ok(())
}

/// `validateAppearanceDictEntry`: a single appearance stream or a subdictionary.
fn validate_appearance_dict_entry(v: &mut Validator<'_>, o: &Object) -> Result<(), Error> {
    let obj = v.x.dereference(o)?.clone();
    match &obj {
        Object::Null => Ok(()),
        Object::Dict(d) => validate_appearance_sub_dict(v, d),
        // Go validates the dereferenced stream through the map it shares with the table: the
        // repairs are stored to the reference.
        Object::Stream(sd) => match o {
            Object::Ref(r) => validate_xobject_stream_dict_contents(v, sd, Some(*r)),
            _ => validate_xobject_stream_dict(v, &obj),
        },
        _ => Err(Error::Validation("unsupported PDF object".to_string())),
    }
}

/// `validateAppearanceEntry`: relaxed validation deletes a corrupt appearance.
fn validate_appearance_entry(
    v: &mut Validator<'_>,
    d: &mut Dict,
    entry_name: &str,
) -> Result<(), Error> {
    let Some(o) = d.find(entry_name.as_bytes()).cloned() else {
        return Ok(());
    };
    match validate_appearance_dict_entry(v, &o) {
        Err(Error::Cancelled) => Err(Error::Cancelled),
        Err(_) => {
            d.delete(entry_name.as_bytes());
            Ok(())
        }
        Ok(()) => Ok(()),
    }
}

/// `validateAppearanceDict` (12.5.5 Appearance Streams): the appearance dictionary `o` is or
/// refers to. A corrupt `N`, `R` or `D` entry is removed. The dictionary is stored back if `o` is
/// a reference; if `o` is a direct dictionary the repaired copy is the result.
///
/// # Errors
/// [`Error::Cancelled`] and the errors of the dereference.
pub fn validate_appearance_dict(v: &mut Validator<'_>, o: &Object) -> Result<Option<Dict>, Error> {
    let r = match o {
        Object::Ref(r) => Some(*r),
        _ => None,
    };
    let Some(mut d) = v.x.dereference_dict(o)?.cloned() else {
        return Ok(None);
    };

    // Normal Appearance: not required in relaxed validation.
    if d.find(b"N").is_some() {
        validate_appearance_entry(v, &mut d, "N")?;
    }

    // Rollover Appearance
    validate_appearance_entry(v, &mut d, "R")?;

    // Down Appearance
    validate_appearance_entry(v, &mut d, "D")?;

    match r {
        Some(r) => {
            store_dict(v.x, r, d);
            Ok(None)
        }
        None => Ok(Some(d)),
    }
}

// ---------------------------------------------------------------------------------------------
// Default appearance strings

/// `strconv.ParseFloat(s, bits)` succeeding (see the module doc for what is not emulated).
fn parse_float_ok(s: &str, bits: u32) -> bool {
    let literal = || {
        let t = s.trim_start_matches(['+', '-']).to_ascii_lowercase();
        t == "inf" || t == "infinity" || t == "nan"
    };
    if bits == 32 {
        s.parse::<f32>().is_ok_and(|f| f.is_finite() || literal())
    } else {
        s.parse::<f64>().is_ok_and(|f| f.is_finite() || literal())
    }
}

/// `validateDARelaxed`: a sequence of page-content graphics or text state operators. At a minimum
/// the string should include a `Tf` operator with its two operands, font and size.
pub fn validate_da_relaxed(s: &str) -> bool {
    let da: Vec<&str> = s.split_whitespace().collect();
    for (i, op) in da.iter().enumerate() {
        match *op {
            "Tf" => {
                if i < 2 || !da[i - 2].starts_with('/') || !parse_float_ok(da[i - 1], 64) {
                    return false;
                }
            }
            "rg" => {
                if i < 3 {
                    return false;
                }
                let r = da[i - 3].strip_prefix('[').unwrap_or(da[i - 3]);
                let b = da[i - 1].strip_suffix(']').unwrap_or(da[i - 1]);
                if !parse_float_ok(r, 32)
                    || !parse_float_ok(da[i - 2], 32)
                    || !parse_float_ok(b, 32)
                {
                    return false;
                }
            }
            "g" if i < 1 || !parse_float_ok(da[i - 1], 32) => {
                return false;
            }
            _ => {}
        }
    }
    true
}

/// `formFieldDARequired` in relaxed validation.
fn form_field_da_required(
    terminal_node: bool,
    out_field_type: Option<&[u8]>,
    requires_da: bool,
) -> bool {
    if matches!(out_field_type, Some(b"Ch")) {
        return OPTIONAL;
    }
    if terminal_node && out_field_type.is_none() {
        return OPTIONAL;
    }
    requires_da
}

/// `validateFormFieldDA` (12.7.4.3): whether the field has a `DA`. A `DA` that is a string is
/// repaired to a string literal.
fn validate_form_field_da(
    x: &mut XRefTable,
    d: &mut Dict,
    dict_name: &str,
    terminal_node: bool,
    out_field_type: Option<&[u8]>,
    requires_da: bool,
) -> Result<bool, Error> {
    let choice_field = matches!(out_field_type, Some(b"Ch"));
    if !(out_field_type.is_none() || matches!(out_field_type, Some(b"Tx")) || choice_field) {
        return Ok(false);
    }

    let required = form_field_da_required(terminal_node, out_field_type, requires_da);
    let da = match validate_string_entry(
        x,
        d,
        dict_name,
        "DA",
        required,
        Version::V10,
        Some(&validate_da_relaxed),
    ) {
        Ok(da) => da,
        Err(e) => {
            if !terminal_node && requires_da {
                return Ok(false);
            }
            return Err(e);
        }
    };
    if let Some(da) = &da {
        // Repair DA
        d.set(b"DA", Object::String(da.clone().into_bytes()));
    }

    Ok(da.is_some_and(|da| !da.is_empty()))
}

// ---------------------------------------------------------------------------------------------
// Signatures

/// `detectRectArray`: the `Rect` of a terminal field, or of the first kid of a field that is not.
fn detect_rect_array(x: &mut XRefTable, d: &Dict, dict_name: &str) -> Result<Array, Error> {
    let Some(obj) = d.find(b"Kids").cloned() else {
        // terminal field
        return Ok(validate_rectangle_entry(
            x,
            d,
            dict_name,
            "Rect",
            REQUIRED,
            Version::V10,
            None,
        )?
        .unwrap_or_default());
    };

    // non terminal field
    let kids = x
        .dereference_array(&obj)
        .map_err(|e| e.context("form field Kids: dereference array"))?
        .cloned()
        .unwrap_or_default();
    let Some(first) = kids.first() else {
        return Err(Error::Validation(
            "form field Kids: empty array".to_string(),
        ));
    };

    let d1 = x
        .dereference_dict(first)
        .map_err(|e| e.context("form field Kids[0]: dereference dict"))?
        .cloned()
        .unwrap_or_default();

    Ok(
        validate_rectangle_entry(x, &d1, dict_name, "Rect", REQUIRED, Version::V10, None)?
            .unwrap_or_default(),
    )
}

/// What [`cache_sig`] keeps per signature: its entry and an increment's map, with B-tree nodes
/// about half full.
const SIG_BYTES: u64 = 2
    * (2 * size_of::<i64>() + size_of::<Signature>() + size_of::<BTreeMap<i64, Signature>>())
        as u64;

/// `cacheSig`: records the signature field `d` (object `obj_nr`, of the increment `incr`), unless
/// it is no signature field.
///
/// # Errors
/// The errors of the lookups of `FT`, `V` and `Rect`.
pub fn cache_sig(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    form: bool,
    obj_nr: i64,
    incr: i64,
) -> Result<(), Error> {
    let (ft, _) = x
        .dereference_name_entry(d, b"FT")
        .map_err(|e| e.context(format!("{dict_name}.FT")))?;
    if !matches!(ft.as_deref(), Some(b"Sig")) {
        return Ok(());
    }

    let page_nr = x.cur_page;
    let mut sig = Signature {
        sig_type: if form { SigType::Form } else { SigType::Page },
        certified: false,
        authoritative: false,
        visible: false,
        signed: d.has_entry(b"V"),
        obj_nr,
        page_nr,
    };

    let mut incr = incr;
    let mut dts = false;

    if let Some(ind_ref) = d.indirect_ref_entry(b"V") {
        let Ok(sig_dict) = x.dereference_dict_ref(ind_ref).map(|d| d.cloned()) else {
            return Ok(());
        };
        let sig_dict = sig_dict.unwrap_or_default();
        // The signature dictionary determines the revision, even when its field was updated later.
        incr = indirect_object_increment(x, Some(&Object::Ref(ind_ref)), incr);
        let (typ, _) = x
            .dereference_name_entry(&sig_dict, b"Type")
            .map_err(|e| e.context("signature dict Type"))?;
        if matches!(typ.as_deref(), Some(b"DocTimeStamp")) {
            sig.sig_type = SigType::Dts;
            dts = true;
        }
    }

    let arr = detect_rect_array(x, d, dict_name)?;

    let r = rect_for_array(x, &arr).map_err(|e| e.context(format!("{dict_name}.Rect")))?;
    sig.visible = r.visible() && !dts;

    let by_obj_nr = x.vstate.signatures.entry(incr).or_default();
    match by_obj_nr.get_mut(&sig.obj_nr) {
        Some(sig1) => sig1.page_nr = page_nr,
        None => {
            // Kept for the table's life: the entry, and the increment's map if it is new.
            x.held.grow_by(SIG_BYTES)?;
            by_obj_nr.insert(sig.obj_nr, sig);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Field dictionaries

/// `validateFormFieldDictEntries` (12.7.3): the effective field type, and whether the field has
/// a `DA`.
fn validate_form_field_dict_entries(
    v: &mut Validator<'_>,
    d: &mut Dict,
    obj_nr: i64,
    incr: i64,
    terminal_node: bool,
    in_field_type: Option<&[u8]>,
    requires_da: bool,
) -> Result<(Option<Vec<u8>>, bool), Error> {
    let dict_name = "formFieldDict";

    // FT: name, Btn,Tx,Ch,Sig. Not required, and any name is valid, in relaxed validation.
    let field_type = validate_name_entry(v.x, d, dict_name, "FT", OPTIONAL, Version::V10, None)?;
    let out_field_type = field_type.or_else(|| in_field_type.map(<[u8]>::to_vec));

    // Parent, required if this is a child in the field hierarchy.
    validate_ind_ref_entry(v.x, d, dict_name, "Parent", OPTIONAL, Version::V10)?;

    // T, optional, text string
    validate_string_entry(v.x, d, dict_name, "T", OPTIONAL, Version::V10, None)?;

    // TU, optional, text string, since V1.3
    validate_string_entry(v.x, d, dict_name, "TU", OPTIONAL, Version::V13, None)?;

    // TM, optional, text string, since V1.3
    validate_string_entry(v.x, d, dict_name, "TM", OPTIONAL, Version::V13, None)?;

    // Ff, optional, integer
    validate_integer_entry(v.x, d, dict_name, "Ff", OPTIONAL, Version::V10, None)?;

    // V, optional, various. The kids are not looked at if V is present.
    validate_entry(v.x, d, dict_name, "V", OPTIONAL, Version::V10)?;
    cache_sig(v.x, d, dict_name, true, obj_nr, incr)?;

    // DV, optional, various
    validate_entry(v.x, d, dict_name, "DV", OPTIONAL, Version::V10)?;

    // AA, optional, dict, since V1.2
    let probe: &Dict = d;
    validate_form_actions_on_page(v, obj_nr, probe, terminal_node, |v: &mut Validator<'_>| {
        validate_additional_actions_with_owner(
            v,
            probe,
            dict_name,
            "AA",
            OPTIONAL,
            Version::V12,
            "fieldOrAnnot",
        )
    })?;

    // DA, required for variable-text fields if not inherited or supplied by the AcroForm
    // dictionary. The default appearance string contains valid page-content graphics or
    // text-state operators that define properties such as the field's text size and colour.
    let has_da = validate_form_field_da(
        v.x,
        d,
        dict_name,
        terminal_node,
        out_field_type.as_deref(),
        requires_da,
    )?;

    Ok((out_field_type, has_da))
}

/// `validateFormFieldParts` (12.7.3): a terminal field, which is merged with its widget
/// annotation.
fn validate_form_field_parts(
    v: &mut Validator<'_>,
    obj_nr: i64,
    incr: i64,
    d: &mut Dict,
    in_field_type: Option<&[u8]>,
    requires_da: bool,
) -> Result<(), Error> {
    // dict represents a terminal field and must have Subtype "Widget"
    if validate_name_entry(
        v.x,
        d,
        "formFieldDict",
        "Subtype",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"Widget"),
    )
    .is_err()
    {
        d.set(b"Subtype", Object::name("Widget"));
    }

    // Validate field dict entries.
    let (field_type, _) =
        validate_form_field_dict_entries(v, d, obj_nr, incr, true, in_field_type, requires_da)?;
    if field_type.is_none() {
        return Err(Error::Validation(
            "form field: missing effective field type".to_string(),
        ));
    }

    // Validate widget annotation - Validation of AA redundant because of merged acrofield with
    // widget annotation.
    let probe = d.clone();
    validate_form_actions_on_page(v, obj_nr, &probe, true, |v: &mut Validator<'_>| {
        validate_annotation_dict(v, d, obj_nr).map(|_| ())
    })
}

/// The context of the messages about the `Kids` of the field `obj_nr`, whose `Kids` entry is
/// `kids_obj_nr` (`formFieldKidsDereferenceError`).
fn kids_context(obj_nr: i64, kids_obj_nr: i64) -> String {
    if kids_obj_nr == obj_nr {
        "form field".to_string()
    } else {
        format!("form field obj#{obj_nr}")
    }
}

/// `FormFieldVisit`: the ancestors of the field being validated, which reject a cycle.
#[derive(Debug, Default)]
struct FormFieldVisit {
    ancestors: BTreeSet<i64>,
}

impl FormFieldVisit {
    fn cycle(obj_nr: i64) -> Error {
        Error::Sentinel(Sentinel::FormFieldCycle).context(format!("obj#{obj_nr}"))
    }

    /// `Enter`.
    fn enter(&mut self, obj_nr: i64) -> Result<(), Error> {
        if obj_nr == 0 {
            return Ok(());
        }
        if !self.ancestors.insert(obj_nr) {
            return Err(Self::cycle(obj_nr));
        }
        Ok(())
    }

    /// `Check`: rejects a cycle without entering `obj_nr`.
    fn check(&self, obj_nr: i64) -> Result<(), Error> {
        if obj_nr != 0 && self.ancestors.contains(&obj_nr) {
            return Err(Self::cycle(obj_nr));
        }
        Ok(())
    }

    /// `Leave`.
    fn leave(&mut self, obj_nr: i64) {
        if obj_nr != 0 {
            self.ancestors.remove(&obj_nr);
        }
    }
}

/// `validateFormFieldKids`: a field that is not terminal.
fn validate_form_field_kids(
    v: &mut Validator<'_>,
    obj_nr: i64,
    incr: i64,
    d: &mut Dict,
    o: &Object,
    in_field_type: Option<&[u8]>,
    requires_da: bool,
    depth: u64,
    visit: &mut FormFieldVisit,
) -> Result<(), Error> {
    // validateNonTerminalFieldSubtype: only the lookup can fail in relaxed validation.
    v.x.dereference_name_entry(d, b"Subtype")
        .map_err(|e| e.context("form field Subtype"))?;

    let kids_obj_nr = match o {
        Object::Ref(r) => r.num,
        _ => obj_nr,
    };
    let context = kids_context(obj_nr, kids_obj_nr);
    let a =
        v.x.dereference_array(o)
            .map_err(|e| e.context(format!("{context}: dereference Kids array")))?
            .cloned()
            .unwrap_or_default();

    // Validate field entries.
    let (x_in_field_type, has_da) =
        validate_form_field_dict_entries(v, d, obj_nr, incr, false, in_field_type, requires_da)?;
    let requires_da = requires_da && !has_da;

    // Recurse over kids.
    for (i, value) in a.iter().enumerate() {
        v.check()?;
        let ir = expect_ref(value, &format!("{context} Kids[{i}]"))?;
        let kid_obj_nr = ir.num;
        let label = format!("form field obj#{obj_nr} Kids[{i}] obj#{kid_obj_nr}");
        visit
            .check(kid_obj_nr)
            .map_err(|e| e.context(label.clone()))?;
        // Relaxed: a kid that cannot be checked counts as validated.
        let valid = v.x.is_valid(ir).unwrap_or(true);
        if !valid {
            validate_form_field_dict_depth(
                v,
                ir,
                x_in_field_type.as_deref(),
                requires_da,
                depth.saturating_add(1),
                visit,
            )
            .map_err(|e| wrap_recursion_error(label, e))?;
        }
    }

    Ok(())
}

/// The part of `validateFormFieldDictDepth` after the visit was entered.
fn validate_form_field_dict_node(
    v: &mut Validator<'_>,
    ir: Ref,
    in_field_type: Option<&[u8]>,
    requires_da: bool,
    depth: u64,
    visit: &mut FormFieldVisit,
) -> Result<(), Error> {
    let obj_nr = ir.num;
    let o = Object::Ref(ir);
    let (d, incr) =
        v.x.dereference_dict_with_incr(&o)
            .map_err(|e| e.context("form field: dereference dict"))?;
    // Relaxed: a missing and an empty dictionary are skipped.
    let Some(mut d) = d.cloned() else {
        return Ok(());
    };
    if d.is_empty() {
        return Ok(());
    }

    v.x.set_valid(ir)
        .map_err(|e| e.context(format!("form field obj#{obj_nr}: mark valid")))?;

    let result = match d.find(b"Kids").cloned() {
        Some(kids) => validate_form_field_kids(
            v,
            obj_nr,
            incr,
            &mut d,
            &kids,
            in_field_type,
            requires_da,
            depth,
            visit,
        ),
        None => validate_form_field_parts(v, obj_nr, incr, &mut d, in_field_type, requires_da),
    };
    store_dict(v.x, ir, d);
    result
}

/// `validateFormFieldDictDepth`: the field `ir`, at `depth` below the `Fields` entry.
fn validate_form_field_dict_depth(
    v: &mut Validator<'_>,
    ir: Ref,
    in_field_type: Option<&[u8]>,
    requires_da: bool,
    depth: u64,
    visit: &mut FormFieldVisit,
) -> Result<(), Error> {
    let obj_nr = ir.num;
    v.x.check_recursion_depth("form field tree", depth)?;
    visit
        .enter(obj_nr)
        .map_err(|e| e.context(format!("form field obj#{obj_nr}")))?;
    let result = validate_form_field_dict_node(v, ir, in_field_type, requires_da, depth, visit);
    visit.leave(obj_nr);
    result
}

/// `validateFormFields`: the fields of the `Fields` or `CO` array.
fn validate_form_fields(
    v: &mut Validator<'_>,
    arr: &[Object],
    requires_da: bool,
) -> Result<(), Error> {
    for (i, value) in arr.iter().enumerate() {
        v.check()?;
        let ir = expect_ref(value, &format!("Fields[{i}]"))?;

        // Relaxed: a field that cannot be checked counts as validated.
        let valid = v.x.is_valid(ir).unwrap_or(true);
        if !valid {
            validate_form_field_dict_depth(
                v,
                ir,
                None,
                requires_da,
                0,
                &mut FormFieldVisit::default(),
            )
            .map_err(|e| e.context(format!("Fields[{i}] obj#{}", ir.num)))?;
        }
    }
    Ok(())
}

/// `validateFormCO` (12.6.3 Trigger Events): an array of references to field dictionaries with
/// calculation actions, since V1.3.
fn validate_form_co(
    v: &mut Validator<'_>,
    arr: &[Object],
    since: Version,
    requires_da: bool,
) -> Result<(), Error> {
    v.x.validate_version("AcroFormCO", since)?;
    validate_form_fields(v, arr, requires_da)
}

// ---------------------------------------------------------------------------------------------
// The interactive form dictionary

/// `validateFormXFAArray` (12.7.8): pairs of a string and a stream.
fn validate_form_xfa_array(x: &mut XRefTable, a: &[Object]) -> Result<(), Error> {
    validate_array_pairs(a, "AcroForm", "XFA", 1)?;
    for (i, v) in a.iter().enumerate() {
        if v.is_null() {
            return Err(Error::Validation(format!(
                "AcroForm XFA[{i}]: missing entry"
            )));
        }
        let o = x
            .dereference(v)
            .map_err(|e| e.context(format!("AcroForm XFA[{i}]: dereference")))?;
        if i.is_multiple_of(2) {
            if string_or_hex_literal(o).is_err() {
                return Err(Error::Validation(format!(
                    "AcroForm XFA[{i}]: expected string"
                )));
            }
            continue;
        }
        if !matches!(o, Object::Stream(_)) {
            return Err(Error::Validation(format!(
                "AcroForm XFA[{i}]: expected stream dict"
            )));
        }
    }
    Ok(())
}

/// `validateFormXFA` (12.7.8): a stream or an array.
fn validate_form_xfa(x: &mut XRefTable, d: &Dict, since: Version) -> Result<(), Error> {
    let Some(raw) = d.find(b"XFA").cloned() else {
        return Ok(());
    };
    let o = x
        .dereference(&raw)
        .map_err(|e| e.context("AcroForm XFA: dereference"))?
        .clone();
    match o {
        Object::Null => {
            return Err(Error::Validation(
                "AcroForm XFA: missing object".to_string(),
            ));
        }
        // no further processing
        Object::Stream(_) => {}
        Object::Array(a) => validate_form_xfa_array(x, &a)?,
        other => {
            return Err(Error::Validation(format!(
                "AcroForm XFA: expected stream dict or array, got {}",
                type_name(&other)
            )));
        }
    }
    x.validate_version("AcroFormXFA", since)
}

/// `validateFormEntryCO`.
fn validate_form_entry_co(
    v: &mut Validator<'_>,
    d: &Dict,
    since: Version,
    requires_da: bool,
) -> Result<(), Error> {
    let Some(o) = d.find(b"CO").cloned() else {
        return Ok(());
    };
    let arr = v.x.dereference_array(&o)?.cloned().unwrap_or_default();
    if arr.is_empty() {
        return Ok(());
    }
    validate_form_co(v, &arr, since, requires_da)
}

/// `validateFormEntryDR`: the default resource dictionary.
fn validate_form_entry_dr(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    let Some(o) = d.find(b"DR").cloned() else {
        return Ok(());
    };
    validate_resource_dict(v, &o)?;
    Ok(())
}

/// `validateFormEntries`: the entries of the interactive form dictionary after `Fields` and `DA`.
fn validate_form_entries(
    v: &mut Validator<'_>,
    d: &Dict,
    dict_name: &str,
    requires_da: bool,
    since: Version,
) -> Result<(), Error> {
    // NeedAppearances: optional, boolean
    validate_boolean_entry(
        v.x,
        d,
        dict_name,
        "NeedAppearances",
        OPTIONAL,
        Version::V10,
        None,
    )?;

    // SigFlags: optional, since 1.3 (relaxed: 1.2), integer
    if let Some(i) =
        validate_integer_entry(v.x, d, dict_name, "SigFlags", OPTIONAL, Version::V12, None)?
    {
        v.x.signature_exist = i & 1 != 0;
        v.x.append_only = i & 2 != 0;
    }

    // CO: array
    validate_form_entry_co(v, d, Version::V13, requires_da)?;

    // DR, optional, resource dict
    validate_form_entry_dr(v, d)?;

    // Q: optional, integer
    validate_integer_entry(
        v.x,
        d,
        dict_name,
        "Q",
        OPTIONAL,
        Version::V10,
        Some(&|i: i64| (0..=2).contains(&i)),
    )?;

    // XFA: optional, since 1.5, stream or array
    validate_form_xfa(v.x, d, since)
}

/// `handleSelfReferentialAcroForm`: an `AcroForm` that refers to the catalog is removed.
fn handle_self_referential_acro_form(x: &XRefTable, root: &mut Dict) -> bool {
    if let Some(ir) = root.indirect_ref_entry(b"AcroForm")
        && x.root == Some(ir)
    {
        root.delete(b"AcroForm");
        return true;
    }
    false
}

/// `nonWidgetAnnotation`: an annotation that is no widget and no field.
fn non_widget_annotation(x: &mut XRefTable, d: &Dict) -> bool {
    let Ok((Some(t), _)) = x.dereference_name_entry(d, b"Type") else {
        return false;
    };
    if t != b"Annot" {
        return false;
    }
    let Ok((Some(st), _)) = x.dereference_name_entry(d, b"Subtype") else {
        return false;
    };
    if st == b"Widget" {
        return false;
    }
    let Ok((None, _)) = x.dereference_name_entry(d, b"FT") else {
        return false;
    };
    d.find(b"Kids").is_none()
}

/// `removeNonWidgetAnnotationsFromFormFields`: the fields without the annotations that are no
/// widgets, and whether there were some.
fn remove_non_widget_annotations_from_form_fields(
    x: &mut XRefTable,
    arr: &[Object],
) -> (Array, bool) {
    let mut cleaned = Array::new();
    let mut removed = false;

    for value in arr {
        if let Object::Ref(ir) = value
            && let Ok(Some(d)) = x.dereference_dict_ref(*ir).map(|d| d.cloned())
            && non_widget_annotation(x, &d)
        {
            removed = true;
            continue;
        }
        cleaned.push(value.clone());
    }

    (cleaned, removed)
}

/// `acroFormFieldsArray`: the `Fields` array `o` of the interactive form dictionary `d`, without
/// the annotations that are no widgets (`d` gets the array if there were some).
fn acro_form_fields_array(x: &mut XRefTable, d: &mut Dict, o: &Object) -> Result<Array, Error> {
    let arr = x
        .dereference_array(o)
        .map_err(|e| e.context("Fields: dereference array"))?
        .cloned()
        .unwrap_or_default();

    let (arr, removed) = remove_non_widget_annotations_from_form_fields(x, &arr);
    if removed {
        d.set(b"Fields", Object::Array(arr.clone()));
    }
    Ok(arr)
}

/// Stores `XRefTable::form` as the `AcroForm` of the catalog: into its object if the catalog's
/// entry is a reference, into `root` (the copy of the catalog that is being validated) if it is a
/// direct dictionary. Go's `XRefTable.Form` is the same map as the `AcroForm` entry.
pub fn sync_acro_form(x: &mut XRefTable, root: &mut Dict) {
    let Some(form) = x.form.clone() else {
        return;
    };
    match root.find(b"AcroForm").cloned() {
        Some(Object::Ref(r)) => store_dict(x, r, form),
        Some(Object::Dict(_)) => root.set(b"AcroForm", Object::Dict(form)),
        _ => {}
    }
}

/// `validateFormContext` (12.7.2 Interactive Form Dictionary): the `AcroForm` entry of the
/// catalog. A form without fields is removed.
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Validation`], the cycle and duplicate errors of the field
/// hierarchy and the errors of the lookups.
pub fn validate_form_context(
    v: &mut Validator<'_>,
    root: &mut Dict,
    _required: bool,
    since: Version,
) -> Result<(), Error> {
    // The page index of the form actions belongs to this pass only.
    let previous = with_form_action_pages(v);
    let result = validate_form_context_pass(v, root, since);
    v.st.form_action_pages = previous;
    result
}

/// `validateFormContext` after `withFormActionPages`.
fn validate_form_context_pass(
    v: &mut Validator<'_>,
    root: &mut Dict,
    since: Version,
) -> Result<(), Error> {
    if handle_self_referential_acro_form(v.x, root) {
        return Ok(());
    }

    let raw_form = root.find(b"AcroForm").cloned();
    let Some(mut d) =
        validate_dict_entry(v.x, root, "rootDict", "AcroForm", OPTIONAL, since, None)?
    else {
        return Ok(());
    };

    // Version check
    v.x.validate_version("AcroForm", since)?;

    // Fields, required, array of indirect references
    let Some(o) = d.find(b"Fields").cloned() else {
        // Fix empty AcroForm dict.
        root.delete(b"AcroForm");
        return Ok(());
    };

    let arr = acro_form_fields_array(v.x, &mut d, &o)?;
    if arr.is_empty() {
        // Fix empty AcroForm dict.
        root.delete(b"AcroForm");
        return Ok(());
    }

    let dict_name = "acroFormDict";

    // DA: optional, string
    let da = validate_string_entry(
        v.x,
        &d,
        dict_name,
        "DA",
        OPTIONAL,
        Version::V10,
        Some(&validate_da_relaxed),
    )?;
    if let Some(da) = &da {
        // Repair
        d.set(b"DA", Object::String(da.clone().into_bytes()));
    }

    let requires_da = da.as_deref().is_none_or(str::is_empty);

    let form_obj_nr = match raw_form {
        Some(Object::Ref(r)) => r.num,
        _ => v.x.root.map_or(0, |r| r.num),
    };
    let fields_obj_nr = match &o {
        Object::Ref(r) => r.num,
        _ => form_obj_nr,
    };
    validate_form_hierarchy_and_fields(v, &arr, fields_obj_nr, requires_da)?;

    validate_form_entries(v, &d, dict_name, requires_da, since)?;

    v.x.form = Some(d);
    sync_acro_form(v.x, root);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Form fields against page annotations

/// `locateAnnForAPAndRect`: the widget annotation with the appearance and the rectangle of the
/// field `d` that comes first by page and object number.
fn locate_ann_for_ap_and_rect(
    d: &Dict,
    r: &Rect,
    page_annots: &BTreeMap<i64, PgAnnots>,
) -> Option<Ref> {
    let ap_obj_nr = d.indirect_ref_entry(b"AP")?.num;
    let rect = rect_short_string(r);
    let mut best: Option<(i64, i64)> = None;
    for (page, m) in page_annots {
        let Some(annots) = m.get(&AnnotationType::Widget) else {
            continue;
        };
        for (candidate, ann) in &annots.map {
            if *candidate <= 0 || ann.rect_string() != rect || ann.ap_obj_nr != ap_obj_nr {
                continue;
            }
            if best.is_none_or(|(p, o)| *page < p || (*page == p && *candidate < o)) {
                best = Some((*page, *candidate));
            }
        }
    }
    best.map(|(_, obj_nr)| Ref {
        num: obj_nr,
        generation: 0,
    })
}

/// `pageAnnotIndRefForAcroField`: the page annotation that stands for the field `ind_ref`.
fn page_annot_ind_ref_for_acro_field(x: &mut XRefTable, ind_ref: Ref) -> Result<Ref, Error> {
    let obj_nr = ind_ref.num;

    // indRef should be part of a page annotation dict.
    for m in x.vstate.page_annots.values() {
        if let Some(annots) = m.get(&AnnotationType::Widget)
            && annots.ind_refs.contains(&ind_ref)
        {
            return Ok(ind_ref);
        }
    }

    // form field is duplicated, retrieve corresponding page annotation for Rect, AP
    let d = x
        .dereference_dict_ref(ind_ref)
        .map_err(|e| {
            e.context(format!(
                "form field obj#{obj_nr}: dereference page annotation candidate"
            ))
        })?
        .cloned()
        .unwrap_or_default();

    let rect_obj = d.0.get(b"Rect".as_slice()).cloned().unwrap_or(Object::Null);
    let Some(arr) = x
        .dereference_array(&rect_obj)
        .map_err(|e| e.context(format!("form field obj#{obj_nr} Rect: dereference array")))?
        .cloned()
    else {
        // Assumption: There are kids and the kids are allright.
        return Ok(ind_ref);
    };

    let r = rect_for_array(x, &arr)?;

    // Possible orphan sig field dicts.
    let (ft, _) = x
        .dereference_name_entry(&d, b"FT")
        .map_err(|e| e.context(format!("form field obj#{obj_nr} FT")))?;
    if matches!(ft.as_deref(), Some(b"Sig")) {
        // Signature Field: without linked sig dict (unsigned), or signed but invisible.
        if d.find(b"V").is_none() || !r.visible() {
            return Ok(ind_ref);
        }
    }

    Ok(locate_ann_for_ap_and_rect(&d, &r, &x.vstate.page_annots).unwrap_or(ind_ref))
}

/// `fixFormFieldsArray`.
fn fix_form_fields_array(x: &mut XRefTable, arr: &[Object]) -> Result<Array, Error> {
    let mut arr1 = Array::new();
    for (i, obj) in arr.iter().enumerate() {
        let ir = expect_ref(obj, &format!("AcroForm Fields[{i}]"))?;
        let ind_ref = page_annot_ind_ref_for_acro_field(x, ir)
            .map_err(|e| e.context(format!("AcroForm Fields[{i}]: resolve page annotation")))?;
        arr1.push(Object::Ref(ind_ref));
    }
    Ok(arr1)
}

/// `validateFormFieldsAgainstPageAnnotations`: a form field that is no page annotation (a
/// duplicate) is replaced by the page annotation with its appearance and rectangle. The repaired
/// `Fields` array is stored in its object, or in a new object if it was direct (then `form` gets
/// the new reference: see [`sync_acro_form`]).
///
/// # Errors
/// [`Error::Validation`] for a `Fields` entry that is no array or reference or an element that is
/// no reference, and the errors of the lookups.
pub fn validate_form_fields_against_page_annotations(x: &mut XRefTable) -> Result<(), Error> {
    let Some(o) = x.form.as_ref().and_then(|f| f.find(b"Fields")).cloned() else {
        return Ok(());
    };

    match o {
        Object::Ref(ind_ref) => {
            let arr = x
                .dereference_array(&o)
                .map_err(|e| e.context("AcroForm Fields: dereference array"))?
                .cloned()
                .unwrap_or_default();
            let arr = fix_form_fields_array(x, &arr)?;
            let Some(entry) = x.entry_mut(ind_ref.num) else {
                return Err(Error::Panic("nil pointer dereference"));
            };
            entry.object = Some(EntryObject::Obj(Object::Array(arr)));
            Ok(())
        }
        Object::Array(arr) => {
            let arr = fix_form_fields_array(x, &arr)?;
            let ind_ref = x.ind_ref_for_new_object(Object::Array(arr)).map_err(|e| {
                e.context("AcroForm Fields: create repaired fields array reference")
            })?;
            if let Some(form) = x.form.as_mut() {
                form.set(b"Fields", Object::Ref(ind_ref));
            }
            Ok(())
        }
        other => Err(Error::Validation(format!(
            "AcroForm Fields: expected array or indirect reference, got {}",
            type_name(&other)
        ))),
    }
}

// ---------------------------------------------------------------------------------------------
// form_hierarchy.go

/// `treeVisit` of the form field hierarchy: rejects cycles and nodes that are reachable from more
/// than one parent.
#[derive(Debug, Default)]
struct FormHierarchyVisit {
    ancestors: BTreeSet<i64>,
    seen: BTreeSet<i64>,
}

impl FormHierarchyVisit {
    /// `enter`: the object number of the node.
    fn enter(&mut self, ir: Ref) -> Result<i64, Error> {
        let n = ir.num;
        if self.ancestors.contains(&n) {
            return Err(Error::Sentinel(Sentinel::FormFieldCycle).context(format!("obj#{n}")));
        }
        if self.seen.contains(&n) {
            return Err(Error::Sentinel(Sentinel::FormFieldDuplicate).context(format!("obj#{n}")));
        }
        self.ancestors.insert(n);
        self.seen.insert(n);
        Ok(n)
    }

    /// `leave`.
    fn leave(&mut self, n: i64) {
        self.ancestors.remove(&n);
    }
}

/// `validateFormHierarchyAndFields`: the structure of the field tree (cycles, fields that are
/// reachable twice), and then the fields. `owner` is the object number of the `Fields` array.
///
/// # Errors
/// [`Error::Cancelled`], [`Sentinel::FormFieldCycle`], [`Sentinel::FormFieldDuplicate`] and the
/// errors of the field validation.
pub fn validate_form_hierarchy_and_fields(
    v: &mut Validator<'_>,
    fields: &[Object],
    owner: i64,
    requires_da: bool,
) -> Result<(), Error> {
    let mut visit = FormHierarchyVisit::default();
    validate_form_hierarchy(v, fields, owner, "Fields", 0, &mut visit)?;
    validate_form_fields(v, fields, requires_da)
}

/// `validateFormHierarchy`.
fn validate_form_hierarchy(
    v: &mut Validator<'_>,
    fields: &[Object],
    owner: i64,
    entry: &str,
    depth: u64,
    visit: &mut FormHierarchyVisit,
) -> Result<(), Error> {
    v.check()?;
    for (i, value) in fields.iter().enumerate() {
        v.check()?;
        let Object::Ref(ir) = value else {
            // Semantic validation retains responsibility for malformed entry types.
            continue;
        };
        if let Err(e) = validate_form_hierarchy_node(v, *ir, depth, visit) {
            let label = format!("form field obj#{owner} {entry}[{i}] obj#{}", ir.num);
            return Err(wrap_recursion_error(label, e));
        }
    }
    Ok(())
}

/// `validateFormHierarchyNode`.
fn validate_form_hierarchy_node(
    v: &mut Validator<'_>,
    ir: Ref,
    depth: u64,
    visit: &mut FormHierarchyVisit,
) -> Result<(), Error> {
    v.check()?;
    v.x.check_recursion_depth("form field tree", depth)?;
    let n = visit.enter(ir)?;
    let result = validate_form_hierarchy_children(v, ir, n, depth, visit);
    visit.leave(n);
    result
}

/// The part of `validateFormHierarchyNode` after the node was entered. Parent, page `Annots` and
/// `CO` are not followed: they refer to nodes rather than owning children.
fn validate_form_hierarchy_children(
    v: &mut Validator<'_>,
    ir: Ref,
    n: i64,
    depth: u64,
    visit: &mut FormHierarchyVisit,
) -> Result<(), Error> {
    // Keep existing strict errors and relaxed recovery in semantic validation.
    let Ok(Some(d)) = v.x.dereference_dict_ref(ir).map(|d| d.cloned()) else {
        return Ok(());
    };
    let Some(raw) = d.find(b"Kids") else {
        return Ok(());
    };
    let Ok(Some(kids)) = v.x.dereference_array(raw).map(|a| a.cloned()) else {
        return Ok(());
    };
    validate_form_hierarchy(v, &kids, n, "Kids", depth.saturating_add(1), visit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::{Annot, Annotation};
    use crate::config::Config;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn table() -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        x
    }

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn r(num: i64) -> Ref {
        Ref { num, generation: 0 }
    }

    fn rect(a: [i64; 4]) -> Object {
        Object::Array(a.iter().map(|i| Object::Integer(*i)).collect())
    }

    /// Go validates an appearance stream through the map it shares with the table: the repair of a
    /// null `BBox` coordinate is stored to the stream object.
    #[test]
    fn appearance_stream_repairs_are_stored() {
        let cancel = AtomicBool::new(false);
        let mut x = table();
        let bbox = Object::Array(vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Null,
            Object::Integer(1),
        ]);
        let sd = crate::object::StreamDict::new(
            dict(&[("Subtype", Object::name("Form")), ("BBox", bbox)]),
            0,
            None,
            None,
            vec![],
        );
        x.table.insert(2, Entry::gen0(Object::Stream(Box::new(sd))));
        let mut v = Validator::new(&mut x, &cancel);
        let ap = dict(&[("N", Object::Ref(r(2)))]);
        validate_appearance_dict(&mut v, &Object::Dict(ap)).unwrap();
        let Some(Entry {
            object: Some(EntryObject::Obj(Object::Stream(sd))),
            ..
        }) = x.table.get(&2)
        else {
            panic!("not a stream");
        };
        let Some(Object::Array(a)) = sd.dict.find(b"BBox") else {
            panic!("no BBox");
        };
        assert!(matches!(a[2], Object::Integer(0)));
    }

    /// `TestFormHierarchyCachedNodes`: the structure is checked even when the semantic validity of
    /// the nodes is cached.
    #[test]
    fn form_hierarchy_cached_nodes() {
        let cancel = AtomicBool::new(false);
        let mut x = table();
        let ir = r(1);
        x.table
            .insert(1, Entry::gen0(Object::Dict(Dict::default())));
        x.set_valid(ir).unwrap();
        let mut v = Validator::new(&mut x, &cancel);
        let e = validate_form_hierarchy_and_fields(
            &mut v,
            &[Object::Ref(ir), Object::Ref(ir)],
            20,
            false,
        )
        .unwrap_err();
        assert!(e.is(Sentinel::FormFieldDuplicate), "got {e}");
    }

    /// `TestFormHierarchyCancellation`: the structure check observes the cancel flag before the
    /// semantic validation reuses what is cached.
    #[test]
    fn form_hierarchy_cancellation() {
        let cancel = AtomicBool::new(false);
        cancel.store(true, Ordering::Relaxed);
        let mut x = table();
        let mut v = Validator::new(&mut x, &cancel);
        let e = validate_form_hierarchy_and_fields(&mut v, &[], 20, false).unwrap_err();
        assert_eq!(e, Error::Cancelled);
    }

    #[test]
    fn a_cycle_in_the_hierarchy_is_found() {
        let cancel = AtomicBool::new(false);
        let mut x = table();
        let kids = Object::Array(vec![Object::Ref(r(1))]);
        x.table
            .insert(1, Entry::gen0(Object::Dict(dict(&[("Kids", kids)]))));
        let mut v = Validator::new(&mut x, &cancel);
        let e = validate_form_hierarchy_and_fields(&mut v, &[Object::Ref(r(1))], 20, false)
            .unwrap_err();
        assert!(e.is(Sentinel::FormFieldCycle), "got {e}");
    }

    #[test]
    fn default_appearance_strings() {
        assert!(validate_da_relaxed("/Helv 12 Tf 0 g"));
        assert!(validate_da_relaxed("/Helv 0 Tf [0 0 1] rg"));
        assert!(validate_da_relaxed("0.5 g"));
        assert!(validate_da_relaxed(""));
        assert!(!validate_da_relaxed("Tf"));
        assert!(!validate_da_relaxed("12 Tf"));
        assert!(!validate_da_relaxed("Helv 12 Tf"));
        assert!(!validate_da_relaxed("/Helv x Tf"));
        assert!(!validate_da_relaxed("0 0 rg"));
        assert!(!validate_da_relaxed("0 0 x rg"));
        assert!(!validate_da_relaxed("g"));
        assert!(!validate_da_relaxed("x g"));
        // Out of range for a float32.
        assert!(!validate_da_relaxed("1e50 g"));
        assert!(validate_da_relaxed("1e50 /F 1 Tf"));
    }

    #[test]
    fn da_requirements_of_a_field() {
        assert!(!form_field_da_required(false, Some(b"Ch"), true));
        assert!(!form_field_da_required(true, None, true));
        assert!(form_field_da_required(false, None, true));
        assert!(form_field_da_required(true, Some(b"Tx"), true));
        assert!(!form_field_da_required(true, Some(b"Tx"), false));
    }

    #[test]
    fn a_field_da_is_repaired_to_a_string_literal() {
        let mut x = table();
        let mut d = dict(&[(
            "DA",
            Object::Hex(b"2f48656c762031322054662030206720".to_vec()),
        )]);
        let has = validate_form_field_da(&mut x, &mut d, "formFieldDict", true, Some(b"Tx"), true)
            .unwrap();
        assert!(has);
        assert!(matches!(d.find(b"DA"), Some(Object::String(s)) if s == b"/Helv 12 Tf 0 g "));
        // A button has none.
        let mut b = Dict::default();
        assert!(
            !validate_form_field_da(&mut x, &mut b, "formFieldDict", true, Some(b"Btn"), true)
                .unwrap()
        );
        // Not terminal and required: an invalid DA is not an error.
        let mut bad = dict(&[("DA", Object::string("junk Tf"))]);
        assert!(
            !validate_form_field_da(&mut x, &mut bad, "formFieldDict", false, Some(b"Tx"), true)
                .unwrap()
        );
        assert!(
            validate_form_field_da(&mut x, &mut bad, "formFieldDict", true, Some(b"Tx"), true)
                .is_err()
        );
    }

    #[test]
    fn signature_fields_are_cached() {
        let mut x = table();
        x.cur_page = 4;
        x.table.insert(
            5,
            Entry::gen0(Object::Dict(dict(&[(
                "Type",
                Object::name("DocTimeStamp"),
            )]))),
        );
        let d = dict(&[
            ("FT", Object::name("Sig")),
            ("V", Object::Ref(r(5))),
            ("Rect", rect([0, 0, 100, 50])),
        ]);
        cache_sig(&mut x, &d, "formFieldDict", true, 7, 3).unwrap();
        // The signature dictionary determines the revision: object 5 is in revision 0.
        let sig = &x.vstate.signatures[&0][&7];
        assert_eq!(sig.sig_type, SigType::Dts);
        assert!(sig.signed && !sig.visible);
        assert_eq!(sig.page_nr, 4);

        let unsigned = dict(&[("FT", Object::name("Sig")), ("Rect", rect([0, 0, 10, 10]))]);
        cache_sig(&mut x, &unsigned, "formFieldDict", false, 8, 3).unwrap();
        let sig = &x.vstate.signatures[&3][&8];
        assert_eq!(sig.sig_type, SigType::Page);
        assert!(!sig.signed && sig.visible);

        // Seen again on another page: only the page changes.
        x.cur_page = 6;
        cache_sig(&mut x, &unsigned, "formFieldDict", true, 8, 3).unwrap();
        let sig = &x.vstate.signatures[&3][&8];
        assert_eq!((sig.sig_type, sig.page_nr), (SigType::Page, 6));

        // No signature, no record.
        let text = dict(&[("FT", Object::name("Tx"))]);
        cache_sig(&mut x, &text, "formFieldDict", true, 9, 3).unwrap();
        assert!(!x.vstate.signatures[&3].contains_key(&9));
    }

    #[test]
    fn xfa_is_a_stream_or_pairs() {
        let mut x = table();
        let pairs = vec![Object::string("template"), Object::Null];
        // A null stream is no stream.
        assert!(validate_form_xfa_array(&mut x, &pairs).is_err());
        let odd = vec![Object::string("a")];
        assert!(validate_form_xfa_array(&mut x, &odd).is_err());
        assert!(validate_form_xfa_array(&mut x, &[]).is_err());
        let d = dict(&[("XFA", Object::Integer(1))]);
        assert!(validate_form_xfa(&mut x, &d, Version::V15).is_err());
        assert!(validate_form_xfa(&mut x, &Dict::default(), Version::V15).is_ok());
    }

    #[test]
    fn a_widget_less_annotation_is_removed_from_the_fields() {
        let mut x = table();
        let annot = dict(&[
            ("Type", Object::name("Annot")),
            ("Subtype", Object::name("Link")),
        ]);
        let widget = dict(&[
            ("Type", Object::name("Annot")),
            ("Subtype", Object::name("Widget")),
        ]);
        x.table.insert(1, Entry::gen0(Object::Dict(annot)));
        x.table.insert(2, Entry::gen0(Object::Dict(widget)));
        let arr = vec![Object::Ref(r(1)), Object::Ref(r(2)), Object::Ref(r(3))];
        let (cleaned, removed) = remove_non_widget_annotations_from_form_fields(&mut x, &arr);
        assert!(removed);
        assert_eq!(cleaned.len(), 2);
        assert!(matches!(&cleaned[0], Object::Ref(q) if q.num == 2));
        assert!(matches!(&cleaned[1], Object::Ref(q) if q.num == 3));
    }

    #[test]
    fn a_self_referential_acro_form_is_removed() {
        let mut x = table();
        x.root = Some(r(1));
        let mut root = dict(&[("AcroForm", Object::Ref(r(1)))]);
        assert!(handle_self_referential_acro_form(&x, &mut root));
        assert!(root.is_empty());
        let mut other = dict(&[("AcroForm", Object::Ref(r(2)))]);
        assert!(!handle_self_referential_acro_form(&x, &mut other));
    }

    /// A field that is no page annotation is replaced by the widget with its appearance and its
    /// rectangle.
    #[test]
    fn a_duplicated_field_is_replaced_by_its_page_annotation() {
        let mut x = table();
        // A read table has the free list head and the size; Go dereferences nil without them.
        x.table.insert(0, Entry::free_head());
        x.size = Some(41);
        // The field, with the appearance 20 and the rectangle of the widget 30.
        let field = dict(&[
            ("FT", Object::name("Tx")),
            ("AP", Object::Ref(r(20))),
            ("Rect", rect([1, 2, 30, 40])),
        ]);
        x.table.insert(10, Entry::gen0(Object::Dict(field)));
        let mut widgets = Annot::default();
        widgets.ind_refs.push(r(30));
        widgets.map.insert(
            30,
            Annotation {
                kind: AnnotationType::Widget,
                rect: Rect::new(1.0, 2.0, 30.0, 40.0),
                ap_obj_nr: 20,
            },
        );
        let mut pg = PgAnnots::new();
        pg.insert(AnnotationType::Widget, widgets);
        x.vstate.page_annots.insert(1, pg);
        // A direct Fields array becomes a new object.
        x.form = Some(dict(&[(
            "Fields",
            Object::Array(vec![Object::Ref(r(10)), Object::Ref(r(30))]),
        )]));
        validate_form_fields_against_page_annotations(&mut x).unwrap();
        let Some(Object::Ref(new)) = x.form.as_ref().and_then(|f| f.find(b"Fields")).cloned()
        else {
            panic!("Fields is a reference now");
        };
        let Some(Entry {
            object: Some(EntryObject::Obj(Object::Array(a))),
            ..
        }) = x.table.get(&new.num)
        else {
            panic!("the repaired array is stored");
        };
        assert!(matches!(&a[..], [Object::Ref(a), Object::Ref(b)] if a.num == 30 && b.num == 30));

        // An indirect Fields array is repaired in place.
        x.table
            .insert(40, Entry::gen0(Object::Array(vec![Object::Ref(r(10))])));
        x.form = Some(dict(&[("Fields", Object::Ref(r(40)))]));
        validate_form_fields_against_page_annotations(&mut x).unwrap();
        assert!(matches!(
            x.table.get(&40).and_then(|e| e.object.as_ref()),
            Some(EntryObject::Obj(Object::Array(a))) if matches!(&a[..], [Object::Ref(q)] if q.num == 30)
        ));

        // Anything else is an error.
        x.form = Some(dict(&[("Fields", Object::Integer(1))]));
        assert!(validate_form_fields_against_page_annotations(&mut x).is_err());
    }

    #[test]
    fn the_acro_form_goes_back_where_it_came_from() {
        let mut x = table();
        let mut form = dict(&[("DA", Object::string("/F 1 Tf"))]);
        x.form = Some(form.clone());

        // A reference: the object.
        x.table
            .insert(5, Entry::gen0(Object::Dict(Dict::default())));
        let mut root = dict(&[("AcroForm", Object::Ref(r(5)))]);
        sync_acro_form(&mut x, &mut root);
        assert!(matches!(
            x.table.get(&5).and_then(|e| e.object.as_ref()),
            Some(EntryObject::Obj(Object::Dict(d))) if d.find(b"DA").is_some()
        ));

        // A direct dictionary: the catalog copy.
        form.set(b"Q", Object::Integer(1));
        x.form = Some(form);
        let mut direct = dict(&[("AcroForm", Object::Dict(Dict::default()))]);
        sync_acro_form(&mut x, &mut direct);
        assert!(matches!(
            direct.find(b"AcroForm"),
            Some(Object::Dict(d)) if d.find(b"Q").is_some()
        ));
    }
}
