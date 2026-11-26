package main

// -gen: writes the inputs of the parity run (design section 12, "-gen mutations"). The corpus is
// every PDF under the data directory, flattened (`a/b.pdf` becomes `a__b.pdf`) and unchanged, the
// pkg/filter/testdata texts for the encode op, and the variants below, seeded by file name so the
// set is the same on every run. The encrypted variants use the snapshot's api.Encrypt, whose
// salts are random: they differ from run to run, but both oracles read the same files.
//
// How much a file gets depends on its size, to bound the number of bytes written:
//
//	up to 300 KiB   everything below
//	up to 3 MiB     generic mutations (fewer bit flips) and the startxref, xref and Size rules
//	larger          four truncations
//
// A variant that needs a feature the file lacks (a /Length, an object stream, an image) is not
// written. The `.limits` sidecar of a boundary variant holds `key=value` lines named after the
// fields of the Rust Limits (see setLimit in main.go).

import (
	"bytes"
	"compress/zlib"
	"context"
	"fmt"
	"hash/fnv"
	"io"
	"math/rand"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"

	"github.com/dobbo-ca/pdf-go/pkg/api"
	"github.com/dobbo-ca/pdf-go/pkg/filter"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu/model"
)

const (
	fullTier = 300 << 10
	midTier  = 3 << 20
	boundMax = 64 << 10 // boundary variants only for files up to this size
)

type generator struct {
	out   string
	count int
}

func (g *generator) emit(name string, data []byte) {
	if err := os.WriteFile(filepath.Join(g.out, name), data, 0o644); err != nil {
		fatalf("%v", err)
	}
	g.count++
}

func (g *generator) emitLimits(name string, kv ...string) {
	if err := os.WriteFile(filepath.Join(g.out, name+".limits"), []byte(strings.Join(kv, "\n")+"\n"), 0o644); err != nil {
		fatalf("%v", err)
	}
}

func gen(dataDir, outDir string) {
	if err := os.MkdirAll(outDir, 0o755); err != nil {
		fatalf("%v", err)
	}
	g := &generator{out: outDir}
	type file struct {
		name string
		data []byte
	}
	var pdfs []file
	var text []byte
	err := filepath.WalkDir(dataDir, func(p string, d os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, _ := filepath.Rel(dataDir, p)
		if d.IsDir() {
			if rel == "goflate" {
				return filepath.SkipDir
			}
			return nil
		}
		ext := strings.ToLower(filepath.Ext(p))
		flat := strings.ReplaceAll(filepath.ToSlash(rel), "/", "__")
		switch {
		case ext == ".pdf":
			data, err := os.ReadFile(p)
			if err != nil {
				return err
			}
			pdfs = append(pdfs, file{flat, data})
		case ext == ".txt" && filepath.Dir(rel) == "filter":
			data, err := os.ReadFile(p)
			if err != nil {
				return err
			}
			g.emit(flat, data)
			if filepath.Base(p) == "pi.txt" {
				text = data
			}
		}
		return nil
	})
	if err != nil {
		fatalf("%v", err)
	}
	for _, f := range pdfs {
		g.emit(f.name, f.data)
		g.mutate(f.name, f.data)
		if len(f.data) <= boundMax {
			g.boundaries(f.name, f.data)
		}
	}
	g.synth(text)
	for _, f := range pdfs {
		switch f.name {
		case "testWithText.pdf", "test.pdf":
			g.encrypted(f.name, f.data)
		}
	}
	fmt.Printf("generated %d files\n", g.count)
}

// ---- mutations of a real file ------------------------------------------------------------

func variant(name, tag string) string {
	return strings.TrimSuffix(name, filepath.Ext(name)) + "." + tag + ".pdf"
}

func (g *generator) mutate(name string, data []byte) {
	n := len(data)
	h := fnv.New64a()
	h.Write([]byte(name))
	rng := rand.New(rand.NewSource(int64(h.Sum64())))

	// Truncations: eight points, among them inside the last xref and inside the first stream.
	cuts := []int{n / 8, n / 4, n / 2, n * 3 / 4, n - 1}
	if i := bytes.LastIndex(data, []byte("xref")); i >= 0 {
		cuts = append(cuts, i+20)
	}
	if i := bytes.Index(data, []byte("stream")); i >= 0 {
		cuts = append(cuts, i+10)
	}
	cuts = append(cuts, n/16)
	if n > midTier {
		cuts = cuts[:4]
	}
	for i, c := range cuts {
		if c > 0 && c < n {
			g.emit(variant(name, "t"+strconv.Itoa(i)), data[:c])
		}
	}
	if n > midTier || n < 2 {
		return
	}

	flips := 16
	if n > fullTier {
		flips = 6
	}
	for i := 0; i < flips; i++ {
		d := append([]byte(nil), data...)
		d[rng.Intn(n)] ^= 1 << rng.Intn(8)
		g.emit(variant(name, "f"+strconv.Itoa(i)), d)
	}

	// Deletions next to `stream` and `endobj`.
	for i, del := range []struct {
		pat  string
		last bool
		off  int // from the match
		len  int
	}{
		{"stream", false, 0, 6}, {"endstream", false, 0, 9}, {"endobj", false, 0, 6},
		{"endobj", true, 0, 6}, {"stream", false, 6, 1}, {"endstream", false, -1, 1},
	} {
		at := bytes.Index(data, []byte(del.pat))
		if del.last {
			at = bytes.LastIndex(data, []byte(del.pat))
		}
		if at < 0 {
			continue
		}
		at += del.off
		if at >= 0 && at+del.len <= n {
			g.emit(variant(name, "d"+strconv.Itoa(i)), append(append([]byte(nil), data[:at]...), data[at+del.len:]...))
		}
	}

	for _, r := range rules {
		if n > fullTier && !r.mid {
			continue
		}
		g.applyRule(name, data, r)
	}

	if n <= fullTier {
		g.streamFlips(name, data, rng)
	}
}

// A rule replaces the capture group of one match of re, once per value.
type rule struct {
	tag  string
	re   *regexp.Regexp
	last bool                  // the last match, not the first
	near string                // search only within 400 bytes of the first occurrence of near
	mid  bool                  // also for files over fullTier
	vals []func(string) string // the new text for the old
}

func lit(s string) func(string) string { return func(string) string { return s } }

func add(d int64) func(string) string {
	return func(old string) string {
		v, err := strconv.ParseInt(strings.TrimSpace(old), 10, 64)
		if err != nil {
			return old
		}
		return strconv.FormatInt(v+d, 10)
	}
}

func add10(d int64) func(string) string {
	return func(old string) string {
		v, err := strconv.ParseInt(old, 10, 64)
		if err != nil || v+d < 0 {
			return old
		}
		return fmt.Sprintf("%010d", v+d)
	}
}

func lits(ss ...string) []func(string) string {
	out := make([]func(string) string, len(ss))
	for i, s := range ss {
		out[i] = lit(s)
	}
	return out
}

func flipHex(old string) string {
	b, err := strconv.ParseUint(old, 16, 8)
	if err != nil {
		return old
	}
	return fmt.Sprintf("%02x", byte(b)^0xff)
}

func rx(s string) *regexp.Regexp { return regexp.MustCompile(s) }

var rules = []rule{
	{tag: "sx", re: rx(`startxref\s+(\d+)`), last: true, mid: true, vals: []func(string) string{add(1), add(-1), lit("0"), lit("99999999999")}},
	{tag: "xo", re: rx(`(\d{10}) \d{5} n`), mid: true, vals: []func(string) string{add10(1), add10(-1), lit("0000000000"), lit("9999999999")}},
	{tag: "sz", re: rx(`/Size (\d+)`), last: true, mid: true, vals: []func(string) string{add(-1), lit("2147483648"), lit("0"), lit("999999999999")}},
	{tag: "ln", re: rx(`/Length (\d+)`), vals: []func(string) string{add(1), add(-1), lit("0"), lit("99999999999"), lit("0 0 R")}},
	{tag: "fl", re: rx(`/Filter\s*(/FlateDecode|\[\s*/FlateDecode\s*\])`), vals: lits("/Foo", "[/JBIG2Decode /FlateDecode]", "[/Crypt /FlateDecode]", "/JBIG2Decode", "/LZWDecode", "[/FlateDecode /FlateDecode]")},
	{tag: "pr", re: rx(`/Predictor (\d+)`), vals: lits("1", "2", "10", "11", "12", "13", "14", "15", "16", "99")},
	{tag: "co", re: rx(`/Colors (\d+)`), vals: lits("0", "-1", "1048576")},
	{tag: "bp", re: rx(`/BitsPerComponent (\d+)`), vals: lits("0", "3", "16", "32")},
	{tag: "cl", re: rx(`/Columns (\d+)`), vals: lits("0", "-1", "1073741824")},
	{tag: "ec", re: rx(`/EarlyChange (\d+)`), vals: lits("0")},
	{tag: "kk", re: rx(`/K (-?\d+)`), vals: lits("1", "-1")},
	{tag: "rw", re: rx(`/Rows (\d+)`), vals: lits("0")},
	{tag: "b1", re: rx(`/BlackIs1 (true|false)`), vals: lits("true", "false")},
	{tag: "ba", re: rx(`/EncodedByteAlign (true|false)`), vals: lits("true", "false")},
	{tag: "on", re: rx(`/N (\d+)`), near: "/ObjStm", vals: []func(string) string{lit("0"), add(1), lit("99999999"), lit("-1")}},
	{tag: "of", re: rx(`/First (\d+)`), near: "/ObjStm", vals: []func(string) string{lit("0"), add(1), lit("99999999"), lit("-1")}},
	{tag: "xw", re: rx(`/W\s*\[([^\]]*)\]`), near: "/XRef", vals: lits("0 0 0", "1 2", "8 8 8", "1 2 1 1")},
	{tag: "rt", re: rx(`(/Root) \d+ 0 R`), last: true, vals: lits("/Rxxt")},
	{tag: "ct", re: rx(`/Type\s*(/Catalog)\b`), vals: lits("/Catalop")},
	{tag: "iw", re: rx(`/Width (\d+)`), vals: lits("0", "-1", "2147483647")},
	{tag: "ih", re: rx(`/Height (\d+)`), vals: lits("0", "-1", "2147483647")},
	{tag: "ics", re: rx(`/ColorSpace\s*(/\w+)`), vals: lits("/DeviceCMYK", "/DeviceGray", "/Pattern")},
	{tag: "idc", re: rx(`/Decode\s*(\[[^\]]*\])`), vals: lits("[1 0]", "[0 1 0 1]")},
	{tag: "ism", re: rx(`/SMask (\d+ \d+ R)`), vals: lits("1 0 R", "99999 0 R")},
	{tag: "ihv", re: rx(`\[\s*/Indexed\s*/\w+\s+(\d+)`), vals: lits("0", "255", "256")},
	{tag: "icn", re: rx(`/N (\d+)\s*/Alternate`), vals: lits("2", "5")},
	{tag: "ica", re: rx(`/Alternate\s*/\w+\s*/N (\d+)`), vals: lits("2", "5")},
}

// encRules edit the encryption dictionary of an encrypted variant.
var encRules = []rule{
	{tag: "eo", re: rx(`/O\s*<([0-9A-Fa-f]{2})`), vals: []func(string) string{flipHex}},
	{tag: "eu", re: rx(`/U\s*<([0-9A-Fa-f]{2})`), vals: []func(string) string{flipHex}},
	{tag: "eoe", re: rx(`/OE\s*<([0-9A-Fa-f]{2})`), vals: []func(string) string{flipHex}},
	{tag: "eue", re: rx(`/UE\s*<([0-9A-Fa-f]{2})`), vals: []func(string) string{flipHex}},
	{tag: "epm", re: rx(`/Perms\s*<([0-9A-Fa-f]{2})`), vals: []func(string) string{flipHex}},
	{tag: "epp", re: rx(`/P (-?\d+)`), vals: []func(string) string{add(1), lit("0")}},
}

func (g *generator) applyRule(name string, data []byte, r rule) {
	lo, hi := 0, len(data)
	if r.near != "" {
		i := bytes.Index(data, []byte(r.near))
		if i < 0 {
			return
		}
		lo, hi = max(0, i-400), min(len(data), i+400)
	}
	ms := r.re.FindAllSubmatchIndex(data[lo:hi], -1)
	if len(ms) == 0 {
		return
	}
	m := ms[0]
	if r.last {
		m = ms[len(ms)-1]
	}
	s, e := lo+m[2], lo+m[3]
	for i, v := range r.vals {
		d := append(append(append([]byte(nil), data[:s]...), v(string(data[s:e]))...), data[e:]...)
		g.emit(variant(name, r.tag+strconv.Itoa(i)), d)
	}
}

// streamFlips flips four bits inside the body of the first stream only.
func (g *generator) streamFlips(name string, data []byte, rng *rand.Rand) {
	i := bytes.Index(data, []byte("stream\n"))
	if i < 0 {
		return
	}
	i += 7
	j := bytes.Index(data[i:], []byte("endstream"))
	if j < 1 {
		return
	}
	for k := 0; k < 4; k++ {
		d := append([]byte(nil), data...)
		d[i+rng.Intn(j)] ^= 1 << rng.Intn(8)
		g.emit(variant(name, "sf"+strconv.Itoa(k)), d)
	}
}

// ---- boundary limits ---------------------------------------------------------------------

// boundaries writes copies of a small file with a `.limits` sidecar that puts max_decode_bytes
// at len-1, len and len+1 of the longest stream, and max_total_decode_bytes at the sum of the
// capacities minus 1 and the sum.
func (g *generator) boundaries(name string, data []byte) {
	ctx, err := readCtx(data, newConf(model.DefaultResourceLimits()))
	if err != nil {
		return
	}
	longest, sum := 0, 0
	conf := newConf(model.DefaultResourceLimits())
	for _, n := range streamsOf(ctx.XRefTable, maxStreams) {
		sd := fresh(ctx.XRefTable, n)
		if _, err := protect(func() (struct{}, error) { return struct{}{}, sd.DecodeWithLimit(conf.DecodeLimit()) }); err != nil {
			continue
		}
		longest = max(longest, len(sd.Content))
		sum += cap(sd.Content)
	}
	if longest == 0 {
		return
	}
	b := func(tag string, kv string) {
		v := variant(name, tag)
		g.emit(v, data)
		g.emitLimits(v, kv)
	}
	for i, d := range []int{-1, 0, 1} {
		if longest+d > 0 {
			b("bd"+strconv.Itoa(i), "max_decode_bytes="+strconv.Itoa(longest+d))
		}
	}
	for i, d := range []int{-1, 0} {
		if sum+d > 0 {
			b("bt"+strconv.Itoa(i), "max_total_decode_bytes="+strconv.Itoa(sum+d))
		}
	}
}

// ---- synthesized PDFs --------------------------------------------------------------------

// buildPDF writes a one-page document whose page content is a stream with the dictionary
// entries `sdict` and the bytes `raw`. `catalog` is added to the catalog dictionary, `extra` are
// further objects numbered from 5.
func buildPDF(catalog, sdict string, raw []byte, extra ...string) []byte {
	var b bytes.Buffer
	b.WriteString("%PDF-1.7\n")
	var offs []int
	obj := func(body string) {
		offs = append(offs, b.Len())
		fmt.Fprintf(&b, "%d 0 obj\n%s\nendobj\n", len(offs), body)
	}
	obj("<< /Type /Catalog /Pages 2 0 R " + catalog + " >>")
	obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
	obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R /Resources << >> >>")
	offs = append(offs, b.Len())
	fmt.Fprintf(&b, "4 0 obj\n<< %s /Length %d >>\nstream\n", sdict, len(raw))
	b.Write(raw)
	b.WriteString("\nendstream\nendobj\n")
	for _, e := range extra {
		obj(e)
	}
	x := b.Len()
	fmt.Fprintf(&b, "xref\n0 %d\n0000000000 65535 f \n", len(offs)+1)
	for _, o := range offs {
		fmt.Fprintf(&b, "%010d 00000 n \n", o)
	}
	fmt.Fprintf(&b, "trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n", len(offs)+1, x)
	return b.Bytes()
}

func encode(name string, parms map[string]int, data []byte) ([]byte, bool) {
	f, err := filter.NewFilter(name, parms)
	if err != nil {
		return nil, false
	}
	r, err := f.Encode(bytes.NewReader(data))
	if err != nil {
		return nil, false
	}
	out, err := io.ReadAll(r)
	return out, err == nil
}

func syncFlate(p []byte, every int) []byte {
	var b bytes.Buffer
	w := zlib.NewWriter(&b)
	for i := 0; i < len(p); i += every {
		_, _ = w.Write(p[i:min(len(p), i+every)])
		_ = w.Flush()
	}
	_ = w.Close()
	return b.Bytes()
}

func (g *generator) synth(text []byte) {
	rng := rand.New(rand.NewSource(1))
	random := make([]byte, 2000)
	rng.Read(random)
	if len(text) > 3000 {
		text = text[:3000]
	}
	payloads := []struct {
		name string
		data []byte
	}{{"text", text}, {"zero", make([]byte, 100000)}, {"rand", random}}

	// A filter x payload x {valid, truncated, corrupt} matrix.
	for _, fn := range []string{"FlateDecode", "LZWDecode", "ASCII85Decode", "ASCIIHexDecode", "RunLengthDecode"} {
		for _, p := range payloads {
			enc, ok := encode(fn, nil, p.data)
			if !ok {
				continue
			}
			bad := append([]byte(nil), enc...)
			for i := 0; i < 3 && len(bad) > 0; i++ {
				bad[rng.Intn(len(bad))] ^= 0x5a
			}
			base := "synth." + strings.TrimSuffix(fn, "Decode") + "." + p.name
			sd := "/Filter /" + fn
			g.emit(base+".valid.pdf", buildPDF("", sd, enc))
			g.emit(base+".trunc.pdf", buildPDF("", sd, enc[:len(enc)*6/10]))
			g.emit(base+".corrupt.pdf", buildPDF("", sd, bad))
		}
	}

	// Two filters in a pipeline.
	if f1, ok := encode("FlateDecode", nil, text); ok {
		if f2, ok := encode("ASCII85Decode", nil, f1); ok {
			g.emit("synth.pipeline.a85flate.pdf", buildPDF("", "/Filter [/ASCII85Decode /FlateDecode]", f2))
		}
		g.emit("synth.pipeline.flateflate.pdf", buildPDF("", "/Filter [/FlateDecode /FlateDecode]", f1))
	}

	// The DecodeParms grid, on Flate data encoded with valid parameters.
	row := make([]byte, 16*3*50)
	rng.Read(row)
	base := map[string]int{"Colors": 3, "BitsPerComponent": 8, "Columns": 16}
	grid := func(tag string, p map[string]int, shown string) {
		parms := map[string]int{"Predictor": p["Predictor"], "Colors": 3, "BitsPerComponent": 8, "Columns": 16}
		enc, ok := encode("FlateDecode", parms, row)
		if !ok {
			return
		}
		g.emit("synth.parms."+tag+".pdf", buildPDF("", "/Filter /FlateDecode /DecodeParms << "+shown+" >>", enc))
	}
	for _, pr := range []int{1, 2, 10, 11, 12, 13, 14, 15, 16, 99} {
		p := map[string]int{"Predictor": pr}
		grid("pred"+strconv.Itoa(pr), p, fmt.Sprintf("/Predictor %d /Colors 3 /BitsPerComponent 8 /Columns 16", pr))
	}
	devs := []struct {
		key string
		val []int
	}{{"Colors", []int{0, -1, 1 << 20}}, {"BitsPerComponent", []int{3, 16, 32}}, {"Columns", []int{0, -1, 1 << 30}}}
	for _, d := range devs {
		for _, v := range d.val {
			parms := map[string]int{"Predictor": 12, "Colors": base["Colors"], "BitsPerComponent": 8, "Columns": 16}
			parms[d.key] = v
			shown := fmt.Sprintf("/Predictor 12 /Colors %d /BitsPerComponent %d /Columns %d", parms["Colors"], parms["BitsPerComponent"], parms["Columns"])
			grid("dev."+d.key+"."+strconv.Itoa(v), map[string]int{"Predictor": 12}, shown)
		}
	}
	if enc, ok := encode("LZWDecode", nil, text); ok {
		g.emit("synth.parms.lzw.early0.pdf", buildPDF("", "/Filter /LZWDecode /DecodeParms << /EarlyChange 0 >>", enc))
		g.emit("synth.parms.lzw.pred2.pdf", buildPDF("", "/Filter /LZWDecode /DecodeParms << /Predictor 2 >>", enc))
	}

	// Flate streams with sync-flush markers (empty stored blocks), for the capacity model.
	big := make([]byte, 100000)
	rng.Read(big)
	for _, every := range []int{1, 1000, 32768, 40000, 100000} {
		for _, p := range []struct {
			name string
			data []byte
		}{{"zero", make([]byte, 100000)}, {"rand", big}} {
			g.emit(fmt.Sprintf("synth.sync.%s.%d.pdf", p.name, every), buildPDF("", "/Filter /FlateDecode", syncFlate(p.data, every)))
		}
	}

	// Nesting to the recursion limit (100) and around it.
	for _, d := range []int{50, 99, 100, 101, 102, 200} {
		g.emit(fmt.Sprintf("synth.nest.arr.%d.pdf", d), buildPDF("/X "+strings.Repeat("[", d)+strings.Repeat("]", d), "", nil))
		g.emit(fmt.Sprintf("synth.nest.dict.%d.pdf", d), buildPDF("/X "+strings.Repeat("<< /A ", d)+"1"+strings.Repeat(" >>", d), "", nil))
	}

	// An object stream document.
	g.objStm()

	// PDF 2.0 documents with core fonts and annotations, for the write path.
	g.pdf20()
}

// objStm writes a document whose pages are in an object stream, found through an xref stream.
func (g *generator) objStm() {
	objs := []string{
		"<< /Type /Catalog /Pages 2 0 R >>",
		"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
		"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> >>",
	}
	var head, body bytes.Buffer
	for i, o := range objs {
		fmt.Fprintf(&head, "%d %d ", i+1, body.Len())
		body.WriteString(o + " ")
	}
	content := append(head.Bytes(), body.Bytes()...)
	zip := func(p []byte) []byte {
		var b bytes.Buffer
		w := zlib.NewWriter(&b)
		_, _ = w.Write(p)
		_ = w.Close()
		return b.Bytes()
	}
	for _, mut := range []struct{ tag, n string }{{"ok", "3"}, {"n0", "0"}, {"n99", "99"}, {"nbig", "999999999"}} {
		var b bytes.Buffer
		b.WriteString("%PDF-1.5\n")
		off4 := b.Len()
		z := zip(content)
		fmt.Fprintf(&b, "4 0 obj\n<< /Type /ObjStm /N %s /First %d /Filter /FlateDecode /Length %d >>\nstream\n", mut.n, head.Len(), len(z))
		b.Write(z)
		b.WriteString("\nendstream\nendobj\n")
		off5 := b.Len()
		// W [1 2 1]: type, field 2, field 3. Objects 0 to 5.
		var x bytes.Buffer
		ent := func(t, a, c int) { x.Write([]byte{byte(t), byte(a >> 8), byte(a), byte(c)}) }
		ent(0, 0, 255)
		ent(2, 4, 0)
		ent(2, 4, 1)
		ent(2, 4, 2)
		ent(1, off4, 0)
		ent(1, off5, 0)
		fmt.Fprintf(&b, "5 0 obj\n<< /Type /XRef /Size 6 /W [1 2 1] /Root 1 0 R /Length %d >>\nstream\n", x.Len())
		b.Write(x.Bytes())
		fmt.Fprintf(&b, "\nendstream\nendobj\nstartxref\n%d\n%%%%EOF\n", off5)
		g.emit("synth.objstm."+mut.tag+".pdf", b.Bytes())
	}
}

// pdfDoc writes a document of the numbered objects `objs` (object 1 is the catalog) with the
// header `header` and the extra trailer entries `trailer`.
func pdfDoc(header string, objs []string, trailer string) []byte {
	var b bytes.Buffer
	b.WriteString(header + "\n")
	var offs []int
	for i, o := range objs {
		offs = append(offs, b.Len())
		fmt.Fprintf(&b, "%d 0 obj\n%s\nendobj\n", i+1, o)
	}
	x := b.Len()
	fmt.Fprintf(&b, "xref\n0 %d\n0000000000 65535 f \n", len(offs)+1)
	for _, o := range offs {
		fmt.Fprintf(&b, "%010d 00000 n \n", o)
	}
	fmt.Fprintf(&b, "trailer\n<< /Size %d /Root 1 0 R %s >>\nstartxref\n%d\n%%%%EOF\n", len(offs)+1, trailer, x)
	return b.Bytes()
}

// pdf20 writes PDF 2.0 documents with core fonts (which the write phase gives explicit metrics,
// writeFont.go) and annotations: the 14 standard fonts' families, an encoding with differences,
// Symbol and ZapfDingbats (with and without a WinAnsi encoding, which has no metrics for most
// glyphs), a font that has its own widths, the 2.0 version in the catalog of a 1.7 header, and
// a document with an information dictionary.
func (g *generator) pdf20() {
	font := func(base, extra string) string {
		return "<< /Type /Font /Subtype /Type1 /BaseFont /" + base + " " + extra + " >>"
	}
	doc := func(fonts ...string) []string {
		res := "<< /Font <<"
		for i := range fonts {
			res += fmt.Sprintf(" /F%d %d 0 R", i+1, 10+i)
		}
		res += " >> >>"
		objs := []string{
			"<< /Type /Catalog /Pages 2 0 R >>",
			"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
			"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources " + res + " /Contents 4 0 R /Annots [5 0 R 6 0 R 7 0 R] >>",
			"<< /Length 35 >>\nstream\nBT /F1 12 Tf 10 10 Td (Hello) Tj ET\nendstream",
			"<< /Type /Annot /Subtype /Link /Rect [10 10 50 20] /Border [0 0 0] /A << /S /URI /URI (https://example.com/) >> >>",
			"<< /Type /Annot /Subtype /Text /Rect [60 60 80 80] /Contents (note) /P 3 0 R >>",
			"<< /Type /Annot /Subtype /Highlight /Rect [5 5 90 15] /QuadPoints [5 15 90 15 5 5 90 5] /C [1 1 0] >>",
			"<< >>", "<< >>",
		}
		return append(objs, fonts...)
	}
	hdr := "%PDF-2.0"
	emit := func(tag, header, trailer string, objs []string) {
		g.emit("synth.pdf20."+tag+".pdf", pdfDoc(header, objs, trailer))
	}
	emit("helvetica", hdr, "", doc(font("Helvetica", "")))
	emit("families", hdr, "", doc(
		font("Helvetica-Bold", ""), font("Times-Roman", ""), font("Times-BoldItalic", ""),
		font("Courier", ""), font("Courier-Oblique", ""), font("Symbol", ""), font("ZapfDingbats", "")))
	emit("winansi", hdr, "", doc(font("Times-Bold", "/Encoding /WinAnsiEncoding"), font("Courier", "/Encoding /MacRomanEncoding")))
	emit("differences", hdr, "", doc(font("Helvetica", "/Encoding << /Type /Encoding /BaseEncoding /WinAnsiEncoding /Differences [65 /Aacute /Agrave 97 /ae] >>")))
	emit("symbol.winansi", hdr, "", doc(font("Symbol", "/Encoding /WinAnsiEncoding")))
	emit("zapf.winansi", hdr, "", doc(font("ZapfDingbats", "/Encoding /WinAnsiEncoding")))
	emit("widths", hdr, "", doc(font("Helvetica", "/FirstChar 32 /LastChar 34 /Widths [278 278 355]")))
	emit("subset", hdr, "", doc(font("ABCDEF+Helvetica", ""), font("Arial", ""), font("Arial,Bold", "")))
	emit("nonfont", hdr, "", doc("<< /Type /Font /Subtype /TrueType /BaseFont /Helvetica >>", "<< /Subtype /Type1 /BaseFont /Helvetica >>"))
	objs := doc(font("Helvetica", ""))
	objs[0] = "<< /Type /Catalog /Pages 2 0 R /Version /2.0 >>"
	emit("rootversion", "%PDF-1.7", "", objs)
	objs = doc(font("Times-Roman", ""))
	objs = append(objs, "<< /Title (T\\344itel) /Author <FEFF00410062> /CreationDate (D:20200101000000Z) >>")
	emit("info", hdr, fmt.Sprintf("/Info %d 0 R", len(objs)), objs)
}

// ---- encrypted variants ------------------------------------------------------------------

// encrypted encrypts base with api.Encrypt under four algorithms and an empty user password, once
// with a non-empty user password (the read must answer wrong-password), and edits the entries of
// the encryption dictionary of the first.
func (g *generator) encrypted(name string, base []byte) {
	type alg struct {
		tag  string
		aes  bool
		bits int
	}
	for _, a := range []alg{{"rc4-40", false, 40}, {"rc4-128", false, 128}, {"aes-128", true, 128}, {"aes-256", true, 256}} {
		for _, user := range []string{"", "user"} {
			if user != "" && a.tag != "aes-256" && a.tag != "rc4-40" {
				continue
			}
			for _, stream := range []bool{false, true} {
				conf := model.NewStatelessConfiguration()
				conf.UserPW, conf.OwnerPW = user, "owner"
				conf.EncryptUsingAES, conf.EncryptKeyLength = a.aes, a.bits
				conf.WriteObjectStream, conf.WriteXRefStream = stream, stream
				var out bytes.Buffer
				_, err := protect(func() (struct{}, error) {
					return struct{}{}, api.Encrypt(context.Background(), bytes.NewReader(base), &out, conf)
				})
				if err != nil {
					continue
				}
				tag := "enc." + a.tag
				if user != "" {
					tag += ".pw"
				}
				if stream {
					tag += ".stm"
				}
				enc := variant(name, tag)
				g.emit(enc, out.Bytes())
				if user == "" && !stream {
					for _, r := range encRules {
						g.applyRule(enc, out.Bytes(), r)
					}
					// The trailer's /Info names the encryption dictionary, so the body reaches it
					// (writeDictObject encrypts it before writeEncryptDict writes it again). The
					// trailer follows the xref table: no offset moves.
					if m := rx(`/Encrypt\s*(\d+ \d+ R)`).FindSubmatch(out.Bytes()); m != nil {
						g.applyRule(enc, out.Bytes(), rule{tag: "ein", re: rx(`/Info\s*(\d+ \d+ R)`), last: true, vals: lits(string(m[1]))})
					}
				}
			}
		}
	}
}
