// Command oracle is the Go side of the pdf-rs parity harness (docs/design.md section 12,
// docs/parity.md). It runs the read-side operations of pdf-go on every input of a directory
// and writes one file per (input, configuration). examples/oracle.rs does the same with the Rust crate, with the
// same configurations, operations, file names and formats, and `tests/parity/diff.sh` compares
// the two output trees.
//
//	oracle -gen <data dir> <input dir>      write the inputs: the corpus and its mutations (gen.go)
//	oracle -genbench <data dir> <bench dir> write the benchmark inputs (bench.go)
//	oracle -bench <runs> <bench dir>        time the benchmark classes, one line per input (bench.go)
//	oracle [-threads N] <input dir> <output dir>
//
// -threads is accepted for symmetry with the Rust oracle and ignored. The oracle builds only
// against the patched upstream (tests/parity/go/build.sh): the run sets pdfcpu.TimeNow and
// pdfcpu.RandReader to the values of WriteEnv::parity() on the Rust side.
//
// Per input `<n>` and configuration `<c>` (d, g, s, and b when `<n>.limits` exists) the file is
// `<n>.<c>.txt`: one section per result, ordered by op then kind, each a header line
// `== <op> <kind>` and the content. An operation that fails as a whole has the kind `err`, holding
// the class token (classify, and the table in docs/parity.md). Raw bytes are written as hs(). Every call runs under recover(): a Go
// panic is the class `panic`, which the Rust side returns as Error::Panic at the same site.
package main

import (
	"bytes"
	"compress/flate"
	"compress/zlib"
	"context"
	"crypto/sha256"
	"encoding/ascii85"
	"encoding/hex"
	"errors"
	"fmt"
	"image/jpeg"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/dobbo-ca/pdf-go/pkg/api"
	"github.com/dobbo-ca/pdf-go/pkg/filter"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu/model"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu/types"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu/validate"
)

// The operations bound how much one input costs.
const (
	maxPages    = 256     // pages op
	maxStreams  = 512     // streams and encode ops
	maxEncodeIn = 4 << 20 // encode op skips a decoded content over this
)

// cfg is one limit configuration (design section 12).
type cfg struct {
	name string
	lim  model.ResourceLimits
}

// configs returns d, g and s, and b when the sidecar `side` (the key=value lines of <n>.limits,
// possibly nil) is present.
func configs(side []byte) []cfg {
	// d: byblos's defaults (byblos/internal/pdfdoc/pdfdoc.go:81-93). StreamBytes is 64 MiB,
	// Objects 200 000, ObjectTableBytes(200 000) is 307 200 000, and MaxParsedBytes is
	// table/4*3 + TotalDecodeBytes = 230 400 000 + 67 108 864. The rest stays at
	// DefaultResourceLimits(), as in byblos.
	d := model.DefaultResourceLimits()
	d.MaxDecodeBytes = 64 << 20
	d.MaxTotalDecodeBytes = 64 << 20
	d.MaxObjectBytes = 64 << 20
	d.MaxObjectCount = 200_000
	d.MaxObjectStreamTotalCount = 200_000
	d.MaxParsedBytes = 297_508_864

	// g: DefaultResourceLimits().
	g := model.DefaultResourceLimits()

	// s: tight limits.
	s := model.DefaultResourceLimits()
	s.MaxDecodeBytes = 64 << 10
	s.MaxTotalDecodeBytes = 256 << 10
	s.MaxParsedBytes = 1 << 20
	s.MaxObjectStreamTotalCount = 1000
	s.MaxStreamTotalBytes = 1 << 20
	s.MaxImagePixels = 1_000_000
	s.MaxObjectBytes = 1 << 20

	out := []cfg{{"d", d}, {"g", g}, {"s", s}}
	if side != nil {
		b := model.DefaultResourceLimits()
		for _, line := range strings.Split(string(side), "\n") {
			line = strings.TrimSpace(line)
			if line == "" {
				continue
			}
			k, v, ok := strings.Cut(line, "=")
			n, err := strconv.ParseInt(v, 10, 64)
			if !ok || err != nil || !setLimit(&b, k, n) {
				fatalf("bad sidecar line %q", line)
			}
		}
		out = append(out, cfg{"b", b})
	}
	return out
}

// setLimit sets the field named by the Rust field name k.
func setLimit(l *model.ResourceLimits, k string, v int64) bool {
	switch k {
	case "max_input_bytes":
		l.MaxInputBytes = v
	case "max_object_bytes":
		l.MaxObjectBytes = v
	case "max_stream_bytes":
		l.MaxStreamBytes = v
	case "max_decode_bytes":
		l.MaxDecodeBytes = v
	case "max_total_decode_bytes":
		l.MaxTotalDecodeBytes = v
	case "max_image_pixels":
		l.MaxImagePixels = v
	case "max_image_bytes":
		l.MaxImageBytes = v
	case "max_object_count":
		l.MaxObjectCount = int(v)
	case "max_object_stream_count":
		l.MaxObjectStreamCount = int(v)
	case "max_object_stream_total_count":
		l.MaxObjectStreamTotalCount = int(v)
	case "max_parsed_bytes":
		l.MaxParsedBytes = v
	case "max_stream_total_bytes":
		l.MaxStreamTotalBytes = v
	case "max_object_stream_first":
		l.MaxObjectStreamFirst = v
	case "max_xref_entries":
		l.MaxXRefEntries = int(v)
	case "max_recursion_depth":
		l.MaxRecursionDepth = int(v)
	default:
		return false
	}
	return true
}

func newConf(l model.ResourceLimits) *model.Configuration {
	conf := model.NewStatelessConfiguration()
	conf.Cmd = model.VALIDATE
	conf.Limits = l
	return conf
}

func fatalf(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "oracle: "+format+"\n", args...)
	os.Exit(1)
}

func main() {
	args := os.Args[1:]
	if len(args) > 0 && args[0] == "-gen" {
		if len(args) != 3 {
			fatalf("usage: oracle -gen <data dir> <input dir>")
		}
		gen(args[1], args[2])
		return
	}
	if len(args) > 0 && args[0] == "-genbench" {
		if len(args) != 3 {
			fatalf("usage: oracle -genbench <data dir> <bench dir>")
		}
		genBench(args[1], args[2])
		return
	}
	if len(args) > 0 && args[0] == "-bench" {
		if len(args) != 3 {
			fatalf("usage: oracle -bench <runs> <bench dir>")
		}
		n, err := strconv.Atoi(args[1])
		if err != nil || n < 1 {
			fatalf("bad run count %q", args[1])
		}
		bench(n, args[2])
		return
	}
	if len(args) > 1 && args[0] == "-threads" {
		args = args[2:]
	}
	if len(args) != 2 {
		fatalf("usage: oracle [-threads N] <input dir> <output dir>")
	}
	run(args[0], args[1])
}

func run(in, out string) {
	// The patched clock and random source (docs/design.md section 10): 2000-01-01T00:00:00Z, and
	// byte i of each request is i mod 256.
	pdfcpu.TimeNow = func() time.Time { return time.Unix(946_684_800, 0).UTC() }
	pdfcpu.RandReader = parityRand{}

	if err := os.MkdirAll(out, 0o755); err != nil {
		fatalf("%v", err)
	}
	build(out)
	ents, err := os.ReadDir(in)
	if err != nil {
		fatalf("%v", err)
	}
	names := make([]string, 0, len(ents))
	for _, e := range ents {
		if !e.IsDir() && !strings.HasSuffix(e.Name(), ".limits") {
			names = append(names, e.Name())
		}
	}
	sort.Strings(names)
	for _, name := range names {
		data, err := os.ReadFile(filepath.Join(in, name))
		if err != nil {
			fatalf("%v", err)
		}
		if strings.HasSuffix(name, ".txt") {
			// pkg/filter/testdata: the encode op only, once.
			encodeText(out, name, data)
			flush(out, name, "g")
			continue
		}
		side, err := os.ReadFile(filepath.Join(in, name+".limits"))
		if err != nil {
			side = nil
		}
		for _, c := range configs(side) {
			doc(out, name, data, c)
			flush(out, name, c.name)
		}
	}
}

// section is one result of one (input, configuration): the old file `<n>.<c>.<op>.<kind>`.
type section struct {
	op, kind string
	data     []byte
}

// pending holds the sections of the (input, configuration) being run, until flush.
var pending []section

// put adds the section (op, kind) to the file of the current (input, configuration).
func put(out, name, c, op, kind string, data []byte) {
	pending = append(pending, section{op, kind, data})
}

// flush writes the pending sections as ONE file `<n>.<c>.txt`, ordered by op then kind
// (bytewise), each as the header line `== <op> <kind>` and the content. Raw bytes are never
// written: the ops that produce them put hs(bytes) instead.
func flush(out, name, c string) {
	sort.SliceStable(pending, func(i, j int) bool {
		if pending[i].op != pending[j].op {
			return pending[i].op < pending[j].op
		}
		return pending[i].kind < pending[j].kind
	})
	var b bytes.Buffer
	for _, s := range pending {
		b.WriteString("== " + s.op + " " + s.kind + "\n")
		b.Write(s.data)
	}
	pending = nil
	if err := os.WriteFile(filepath.Join(out, name+"."+c+".txt"), b.Bytes(), 0o644); err != nil {
		fatalf("%v", err)
	}
}

func putErr(out, name, c, op string, err error) {
	put(out, name, c, op, "err", []byte(classify(err)+"\n"))
}

// errGoPanic stands for a recovered Go panic.
var errGoPanic = errors.New("go panic")

// protect runs f under recover().
func protect[T any](f func() (T, error)) (v T, err error) {
	defer func() {
		if r := recover(); r != nil {
			var zero T
			v, err = zero, errGoPanic
		}
	}()
	return f()
}

// readCtx is api.ReadContext on data (pkg/api/api.go:81).
func readCtx(data []byte, conf *model.Configuration) (*model.Context, error) {
	return protect(func() (*model.Context, error) {
		return api.ReadContext(context.Background(), bytes.NewReader(data), conf)
	})
}

// doc runs the read, pages, streams, budget and encode operations of one input under one
// configuration. The read context is read twice: once for read, pages, streams and encode, and
// once, untouched, for budget (whose result depends on the budget state a read leaves).
func doc(out, name string, data []byte, c cfg) {
	conf := newConf(c.lim)
	ctx, err := readCtx(data, conf)
	if err != nil {
		putErr(out, name, c.name, "read", err)
		return
	}
	body, err := protect(func() ([]byte, error) { return dumpRead(ctx), nil })
	if err != nil {
		putErr(out, name, c.name, "read", err)
		return
	}
	put(out, name, c.name, "read", "dump", body)

	if pg, err := protect(func() ([]byte, error) { return opPages(ctx), nil }); err != nil {
		putErr(out, name, c.name, "pages", err)
	} else {
		put(out, name, c.name, "pages", "out", pg)
	}

	streams(out, name, c, conf, ctx)

	if ctx2, err := readCtx(data, newConf(c.lim)); err != nil {
		putErr(out, name, c.name, "budget", err)
	} else if b, err := protect(func() ([]byte, error) { return opBudget(ctx2), nil }); err != nil {
		putErr(out, name, c.name, "budget", err)
	} else {
		put(out, name, c.name, "budget", "out", b)
	}

	moreOps(out, name, data, c)
}

// numbers returns the object numbers of the table, ascending.
func numbers(t *model.XRefTable) []int {
	ns := make([]int, 0, len(t.Table))
	for n := range t.Table {
		ns = append(ns, n)
	}
	sort.Ints(ns)
	return ns
}

func hs(b []byte) string {
	h := sha256.Sum256(b)
	return strconv.Itoa(len(b)) + ":" + hex.EncodeToString(h[:])
}

func hsNil(b []byte) string {
	if b == nil {
		return "nil"
	}
	return hs(b)
}

func ptrInt(p *int) string {
	if p == nil {
		return "nil"
	}
	return strconv.Itoa(*p)
}

func refStr(r *types.IndirectRef) string {
	if r == nil {
		return "nil"
	}
	return fmt.Sprintf("%d %d", r.ObjectNumber.Value(), r.GenerationNumber.Value())
}

func verStr(v *model.Version) string {
	if v == nil {
		return "nil"
	}
	return v.String()
}

// fmtF is a float as the shortest decimal that round-trips, with no exponent.
func fmtF(f float64) string {
	return strconv.FormatFloat(f, 'f', -1, 64)
}

func rectStr(r *types.Rectangle) string {
	if r == nil {
		return "nil"
	}
	return fmtF(r.LL.X) + " " + fmtF(r.LL.Y) + " " + fmtF(r.UR.X) + " " + fmtF(r.UR.Y)
}

// dumpRead is the canonical dump of a context (design section 12, op read).
func dumpRead(ctx *model.Context) []byte {
	var b bytes.Buffer
	t := ctx.XRefTable
	fmt.Fprintf(&b, "size %s\n", ptrInt(t.Size))
	fmt.Fprintf(&b, "root %s\n", refStr(t.Root))
	fmt.Fprintf(&b, "info %s\n", refStr(t.Info))
	if t.ID == nil {
		b.WriteString("id nil\n")
	} else {
		fmt.Fprintf(&b, "id %s\n", t.ID.PDFString())
	}
	fmt.Fprintf(&b, "encrypt %s\n", refStr(t.Encrypt))
	if t.E == nil {
		b.WriteString("enc nil\n")
	} else {
		fmt.Fprintf(&b, "enc p=%d v=%d r=%d\n", t.E.P, t.E.V, t.E.R)
	}
	fmt.Fprintf(&b, "header %s\n", verStr(t.HeaderVersion))
	fmt.Fprintf(&b, "rootversion %s\n", verStr(t.RootVersion))
	fmt.Fprintf(&b, "pagecount %d\n", t.PageCount)
	fmt.Fprintf(&b, "maxobjnr %d\n", t.MaxObjNr)
	fmt.Fprintf(&b, "objstm %t\n", ctx.Read != nil && ctx.Read.UsingObjectStreams)
	for _, n := range numbers(t) {
		e := t.Table[n]
		fmt.Fprintf(&b, "obj %d gen %s free %t ", n, ptrInt(e.Generation), e.Free)
		switch o := e.Object.(type) {
		case nil:
			b.WriteString("nil")
		case types.StreamDict:
			fmt.Fprintf(&b, "stream %s raw %s content %s", o.PDFString(), hsNil(o.Raw), hsNil(o.Content))
		case types.ObjectStreamDict:
			// No content: the lazy entries decode the stream on their own copy.
			fmt.Fprintf(&b, "objstm %s raw %s n %d first %d", o.PDFString(), hsNil(o.Raw), o.ObjCount, o.FirstObjOffset)
		case types.XRefStreamDict:
			fmt.Fprintf(&b, "xrefstm %s raw %s size %d w %d %d %d", o.PDFString(), hsNil(o.Raw), o.Size, o.W[0], o.W[1], o.W[2])
		case types.LazyObjectStreamObject:
			data, err := o.GetData()
			if err != nil {
				fmt.Fprintf(&b, "lazy err %s", classify(err))
			} else {
				fmt.Fprintf(&b, "lazy %s", hs(data))
			}
		default:
			b.WriteString(o.PDFString())
		}
		b.WriteByte('\n')
	}
	return b.Bytes()
}

// opPages is the pages operation: EnsurePageCount, PageDict(n, false) and the inherited
// attributes for up to maxPages pages.
func opPages(ctx *model.Context) []byte {
	var b bytes.Buffer
	t := ctx.XRefTable
	if err := t.EnsurePageCount(); err != nil {
		fmt.Fprintf(&b, "count err %s\n", classify(err))
		return b.Bytes()
	}
	fmt.Fprintf(&b, "count %d\n", t.PageCount)
	for p := 1; p <= t.PageCount && p <= maxPages; p++ {
		line, err := protect(func() (string, error) {
			d, ref, attrs, err := t.PageDict(context.Background(), p, false)
			if err != nil {
				return "", err
			}
			res := "nil"
			if attrs.Resources != nil {
				res = attrs.Resources.PDFString()
			}
			return fmt.Sprintf("ref %s dict %s res %s mb %s cb %s rot %d",
				refStr(ref), d.PDFString(), res, rectStr(attrs.MediaBox), rectStr(attrs.CropBox), attrs.Rotate), nil
		})
		if err != nil {
			fmt.Fprintf(&b, "page %d err %s\n", p, classify(err))
			continue
		}
		fmt.Fprintf(&b, "page %d %s\n", p, line)
	}
	return b.Bytes()
}

// streamsOf returns the object numbers of the plain stream dictionaries, ascending, up to max.
func streamsOf(t *model.XRefTable, max int) []int {
	var ns []int
	for _, n := range numbers(t) {
		if _, ok := t.Table[n].Object.(types.StreamDict); ok {
			if len(ns) == max {
				break
			}
			ns = append(ns, n)
		}
	}
	return ns
}

// fresh is a copy of the stream of object n that has not been decoded.
func fresh(t *model.XRefTable, n int) types.StreamDict {
	sd := t.Table[n].Object.(types.StreamDict)
	sd.Content = nil
	return sd
}

// streams runs the streams and encode operations: DecodeWithLimit on a fresh copy of up to
// maxStreams streams, then NewStreamDictForBuf and Encode on each decoded content.
func streams(out, name string, c cfg, conf *model.Configuration, ctx *model.Context) {
	t := ctx.XRefTable
	for _, n := range streamsOf(t, maxStreams) {
		sd := fresh(t, n)
		_, err := protect(func() (struct{}, error) { return struct{}{}, sd.DecodeWithLimit(conf.DecodeLimit()) })
		op := "s" + strconv.Itoa(n)
		if err != nil {
			put(out, name, c.name, op, "out", []byte("err "+classify(err)+"\n"))
			continue
		}
		put(out, name, c.name, op, "out", []byte("ok "+hs(sd.Content)+"\n"))
		put(out, name, c.name, op, "cap", []byte(strconv.Itoa(cap(sd.Content))+"\n"))
		if len(sd.Content) <= maxEncodeIn {
			buf := append([]byte(nil), sd.Content...)
			put(out, name, c.name, "e"+strconv.Itoa(n), "out", encodeBuf(t, buf))
		}
	}
}

// encodeBuf is NewStreamDictForBuf and Encode on buf, as a result line.
func encodeBuf(t *model.XRefTable, buf []byte) []byte {
	line, err := protect(func() (string, error) {
		nsd, err := t.NewStreamDictForBuf(buf)
		if err != nil {
			return "", err
		}
		if err := nsd.Encode(); err != nil {
			return "", err
		}
		return fmt.Sprintf("raw %s length %s", hs(nsd.Raw), ptrInt(nsd.IntEntry("Length"))), nil
	})
	if err != nil {
		return []byte("err " + classify(err) + "\n")
	}
	return []byte(line + "\n")
}

// encodeText is the encode op on a pkg/filter/testdata file, and the zlevel op: zlib at
// BestSpeed (1) and BestCompression (9).
func encodeText(out, name string, data []byte) {
	put(out, name, "g", "e0", "out", encodeBuf(&model.XRefTable{}, data))
	for _, l := range []int{zlib.BestSpeed, zlib.BestCompression} {
		put(out, name, "g", "zlevel", strconv.Itoa(l), zlibLevel(data, l))
	}
}

// zlibLevel is zlib.NewWriterLevel, one Write and Close on data, as a result line.
func zlibLevel(data []byte, level int) []byte {
	var b bytes.Buffer
	w, err := zlib.NewWriterLevel(&b, level)
	if err == nil {
		_, err = w.Write(data)
	}
	if err == nil {
		err = w.Close()
	}
	if err != nil {
		return []byte("err " + classify(err) + "\n")
	}
	return []byte(hs(b.Bytes()) + "\n")
}

// opBudget runs XRefTable.DecodeStream over all plain streams in object order. The table's
// cumulative decode budget carries from one stream to the next.
func opBudget(ctx *model.Context) []byte {
	var b bytes.Buffer
	t := ctx.XRefTable
	for _, n := range streamsOf(t, int(^uint(0)>>1)) {
		sd := fresh(t, n)
		_, err := protect(func() (struct{}, error) { return struct{}{}, t.DecodeStream(&sd) })
		if err != nil {
			fmt.Fprintf(&b, "%d %s\n", n, classify(err))
			continue
		}
		fmt.Fprintf(&b, "%d ok %d\n", n, cap(sd.Content))
	}
	return b.Bytes()
}

// classify is the class token of an error, the Go side of Error::class (src/error.rs, design
// section 5): from the most specific case down, and the fallback `syntax` for the untyped errors
// that the Rust side calls Corrupt or Sentinel.
func classify(err error) string {
	var (
		fe flate.CorruptInputError
		ae ascii85.CorruptInputError
		he hex.InvalidByteError
		je jpeg.FormatError
		ju jpeg.UnsupportedError
	)
	msg := err.Error()
	switch {
	case errors.Is(err, errGoPanic):
		return "panic"
	case errors.Is(err, pdfcpu.ErrWrongPassword):
		return "wrong-password"
	case errors.Is(err, model.ErrObjectBytesExceeded):
		return "limit/object-bytes"
	case errors.Is(err, model.ErrParsedLimitExceeded):
		return "limit/parsed"
	case errors.Is(err, model.ErrStreamTotalLimitExceeded):
		return "limit/stream-total"
	case errors.Is(err, model.ErrLimitExceeded) && errors.Is(err, filter.ErrDecodeLimitExceeded):
		return "limit/total-decode"
	case errors.Is(err, model.ErrLimitExceeded) && strings.Contains(msg, "object stream entries"):
		return "limit/objstm-entries"
	case errors.Is(err, model.ErrLimitExceeded):
		return "limit/objects"
	case errors.Is(err, filter.ErrDecodeLimitExceeded):
		return "limit/decode"
	case strings.Contains(msg, "xref subsection object range exceeds limit"):
		return "limit/xref-range"
	case strings.Contains(msg, "pixel count") && strings.Contains(msg, "exceeds limit"):
		return "limit/image-pixels"
	case strings.Contains(msg, "byte size exceeds limit"):
		return "limit/image-bytes"
	case errors.Is(err, model.ErrNoContent):
		return "no-content"
	case errors.Is(err, filter.ErrUnsupportedFilter):
		return "unsupported-filter"
	case strings.Contains(msg, "invalid filter: <"):
		return "invalid-filter"
	case errors.Is(err, pdfcpu.ErrUnsupportedResource):
		return "unsupported-resource"
	case errors.Is(err, pdfcpu.ErrUnsupportedEncryptionFeature):
		return "encryption/unsupported"
	case errors.Is(err, pdfcpu.ErrMalformedEncryption):
		return "encryption/malformed"
	case errors.As(err, &fe):
		return "flate/corrupt"
	case errors.Is(err, zlib.ErrHeader):
		return "zlib/header"
	case errors.Is(err, zlib.ErrChecksum):
		return "flate/checksum"
	case errors.Is(err, zlib.ErrDictionary):
		return "zlib/dictionary"
	case errors.Is(err, io.ErrUnexpectedEOF):
		return "unexpected-eof"
	case strings.Contains(msg, "lzw: invalid code"):
		return "lzw/invalid-code"
	case errors.As(err, &ae):
		return "ascii85/corrupt"
	case strings.Contains(msg, "ASCII85 decode: missing eod marker"):
		return "ascii85/eod"
	case errors.As(err, &he):
		return "hex/invalid-byte"
	case errors.As(err, &ju):
		return "jpeg/unsupported"
	case errors.As(err, &je):
		return "jpeg/format"
	case strings.Contains(msg, "CCITT decode") || strings.Contains(msg, "ccitt:"):
		return "ccitt"
	case strings.Contains(msg, "gob:"):
		return "gob"
	case isRows(msg):
		return "predictor/rows"
	case isParams(msg):
		return "predictor/params"
	case errors.Is(err, context.Canceled):
		return "cancelled"
	case isSentinel(err):
		return "syntax"
	case strings.Contains(msg, "validation error"):
		// api.validationError's prefix: an untyped error of the validation pass.
		return "validate"
	}
	return "syntax"
}

// sentinels are the Go errors that the Rust crate has as Sentinel variants (src/error.rs): they
// stay `syntax` when validation wraps them.
var sentinels = []error{
	model.ErrActionCycle, model.ErrAnnotationIRTCycle, model.ErrBeadCycle, model.ErrCMapCycle,
	model.ErrColorSpaceCycle, model.ErrFormFieldCycle, model.ErrFormFieldDuplicate,
	model.ErrFunctionCycle, model.ErrHalftoneCycle, model.ErrMaxRecursionDepthExceeded,
	model.ErrMediaClipCycle, model.ErrNameTreeCycle, model.ErrNameTreeDuplicate,
	model.ErrNumberTreeCycle, model.ErrNumberTreeDuplicate, model.ErrPageTreeCycle,
	model.ErrPageTreeDuplicate, model.ErrPatternCycle, model.ErrRenditionCycle,
	model.ErrStructureTreeCycle, model.ErrStructureTreeDuplicate, model.ErrTargetCycle,
	model.ErrPageNotFound, model.ErrExpectedDict, model.ErrMissingEncryptDictObject,
	model.ErrWrongTypeEncryptDictObject, model.ErrCorruptObjectOffset,
	model.ErrXRefStreamIndexSizeMismatch, pdfcpu.ErrReferenceDoesNotExist,
	validate.ErrMissingFont, validate.ErrBookmarksRepair,
}

func isSentinel(err error) bool {
	for _, s := range sentinels {
		if errors.Is(err, s) {
			return true
		}
	}
	msg := err.Error()
	for _, s := range []string{"corrupt page content", "corrupt TJ expression", "corrupt BI expression"} {
		if strings.Contains(msg, s) {
			return true
		}
	}
	return false
}

func isRows(msg string) bool {
	for _, s := range []string{
		"flate decode: unexpected PNG predictor",
		"flate decode: postprocessing failed",
		"flate decode: read error, expected",
	} {
		if strings.Contains(msg, s) {
			return true
		}
	}
	return false
}

func isParams(msg string) bool {
	for _, s := range []string{
		"flate decode: undefined \"Predictor\"",
		"flate decode: \"Colors\" must be",
		"flate decode: \"Columns\" must be",
		"flate decode: unexpected \"BitsPerComponent\"",
		"flate encode: TIFF predictor",
		"LZW decode: unsupported predictor",
	} {
		if strings.Contains(msg, s) {
			return true
		}
	}
	return false
}
