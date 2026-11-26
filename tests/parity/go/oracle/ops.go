// The operations of the second half of the parity harness (docs/design.md section 12, WP 27):
// images, validate, rvo, propsadd, props, write, writeopt, and the input-independent build op.
// examples/oracle.rs has the same operations, file names and formats.
package main

import (
	"bytes"
	"context"
	"encoding/hex"
	"fmt"
	"io"
	"sort"
	"strconv"
	"strings"

	"github.com/dobbo-ca/pdf-go/pkg/api"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu/model"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu/types"
)

// maxImages bounds the images op.
const maxImages = 64

// propsAdd is what the propsadd op adds (design section 12).
var propsAdd = map[string]string{"Title": "parity", "Key": "Wert ü"}

// parityRand is the patched pdfcpu.RandReader: byte i of every request is i mod 256, as
// WriteEnv::parity() on the Rust side.
type parityRand struct{}

func (parityRand) Read(p []byte) (int, error) {
	for i := range p {
		p[i] = byte(i % 256)
	}
	return len(p), nil
}

// result puts the result of f as the section `<op> <ext>`, or its error as `<op> err`.
func result(out, name, c, op, ext string, f func() ([]byte, error)) {
	b, err := protect(f)
	if err != nil {
		putErr(out, name, c, op, err)
		return
	}
	put(out, name, c, op, ext, b)
}

// moreOps runs the operations that start from the bytes of the input, under one configuration.
// Each starts its own read, so none sees the state another left behind.
func moreOps(out, name string, data []byte, c cfg) {
	result(out, name, c.name, "validate", "ok", func() ([]byte, error) {
		err := api.Validate(context.Background(), bytes.NewReader(data), newConf(c.lim), nil)
		return []byte("ok\n"), err
	})
	result(out, name, c.name, "rvo", "dump", func() ([]byte, error) {
		ctx, err := rvo(data, newConf(c.lim))
		if err != nil {
			return nil, err
		}
		return dumpRvo(ctx), nil
	})
	result(out, name, c.name, "rav", "dump", func() ([]byte, error) {
		ctx, err := api.ReadAndValidate(context.Background(), bytes.NewReader(data), newConf(c.lim))
		if err != nil {
			return nil, err
		}
		return dumpRead(ctx), nil
	})
	result(out, name, c.name, "propsadd", "dump", func() ([]byte, error) {
		conf := newConf(c.lim)
		conf.Cmd = model.ADDPROPERTIES
		ctx, err := rvo(data, conf)
		if err != nil {
			return nil, err
		}
		if err := pdfcpu.PropertiesAdd(context.Background(), ctx, propsAdd); err != nil {
			return nil, err
		}
		return dumpRead(ctx), nil
	})
	result(out, name, c.name, "props", "out", func() ([]byte, error) {
		m, err := api.Properties(context.Background(), bytes.NewReader(data), newConf(c.lim))
		if err != nil {
			return nil, err
		}
		return propsText(m), nil
	})
	opWrite(out, name, data, c, "write", func() (*model.Context, error) {
		return api.ReadContext(context.Background(), bytes.NewReader(data), newConf(c.lim))
	})
	opWrite(out, name, data, c, "writeopt", func() (*model.Context, error) {
		return rvo(data, newConf(c.lim))
	})
	opImages(out, name, data, c)
}

// rvo is api.ReadValidateAndOptimize (pkg/api/api.go:304).
func rvo(data []byte, conf *model.Configuration) (*model.Context, error) {
	return api.ReadValidateAndOptimize(context.Background(), bytes.NewReader(data), conf, nil)
}

// dumpRvo is the read dump of an optimized context, the keys of the name tree cache, and whether
// the form dictionary is set.
func dumpRvo(ctx *model.Context) []byte {
	var b bytes.Buffer
	b.Write(dumpRead(ctx))
	t := ctx.XRefTable
	names := make([]string, 0, len(t.Names))
	for k := range t.Names {
		names = append(names, k)
	}
	sort.Strings(names)
	fmt.Fprintf(&b, "names %s\n", strings.Join(names, ","))
	fmt.Fprintf(&b, "form %t\n", t.Form != nil)
	return b.Bytes()
}

// propsText is the sorted `k=v` lines of the properties, with backslash, newline and carriage
// return escaped.
func propsText(m map[string]string) []byte {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	esc := strings.NewReplacer("\\", "\\\\", "\n", "\\n", "\r", "\\r")
	var b bytes.Buffer
	for _, k := range keys {
		b.WriteString(esc.Replace(k) + "=" + esc.Replace(m[k]) + "\n")
	}
	return b.Bytes()
}

// writeCtx is api.WriteContext (pkg/api/api.go:169) into memory.
func writeCtx(ctx *model.Context) ([]byte, error) {
	var buf bytes.Buffer
	if err := api.WriteContext(context.Background(), ctx, &buf); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}

// opWrite is read (ReadContext for `write`, ReadValidateAndOptimize for `writeopt`) then WriteContext:
// the hs of the exact bytes (`<op>.out`) and the dump of a read of the output (`<op>.redump`,
// `err <class>` if that read fails).
func opWrite(out, name string, data []byte, c cfg, op string, read func() (*model.Context, error)) {
	bb, err := protect(func() ([]byte, error) {
		ctx, err := read()
		if err != nil {
			return nil, err
		}
		return writeCtx(ctx)
	})
	if err != nil {
		putErr(out, name, c.name, op, err)
		return
	}
	put(out, name, c.name, op, "out", []byte(hs(bb)+"\n"))
	ctx, err := readCtx(bb, newConf(c.lim))
	if err == nil {
		var dump []byte
		if dump, err = protect(func() ([]byte, error) { return dumpRead(ctx), nil }); err == nil {
			put(out, name, c.name, op, "redump", dump)
			return
		}
	}
	put(out, name, c.name, op, "redump", []byte("err "+classify(err)+"\n"))
}

// opImages is ColorSpaceComponents and ExtractImage on a copy of each image stream (up to
// maxImages, ascending), after a read of its own. `images.out` has a line per image and
// `i<obj>.<type>.out` the length and hash of each rendered file.
func opImages(out, name string, data []byte, c cfg) {
	ctx, err := readCtx(data, newConf(c.lim))
	if err != nil {
		putErr(out, name, c.name, "images", err)
		return
	}
	result(out, name, c.name, "images", "out", func() ([]byte, error) {
		var b bytes.Buffer
		t := ctx.XRefTable
		count := 0
		for _, n := range numbers(t) {
			e := t.Table[n]
			if e == nil {
				continue
			}
			sd0, ok := e.Object.(types.StreamDict)
			if !ok {
				continue
			}
			if st := sd0.Subtype(); st == nil || *st != "Image" {
				continue
			}
			if count == maxImages {
				break
			}
			count++
			sd := sd0.Clone().(types.StreamDict)
			sd.Content = nil
			// Clone makes a nil FilterPipeline empty, which img (extract.go) takes for a filter
			// it does not know; pdf-go's callers pass the table's own dict, so keep it nil.
			if sd0.FilterPipeline == nil {
				sd.FilterPipeline = nil
			}
			fmt.Fprintf(&b, "image %d comps ", n)
			if k, err := protect(func() (int, error) { return pdfcpu.ColorSpaceComponents(t, &sd) }); err != nil {
				fmt.Fprintf(&b, "err %s", classify(err))
			} else {
				fmt.Fprintf(&b, "%d", k)
			}
			type rendered struct {
				typ  string
				data []byte
			}
			img, err := protect(func() (*rendered, error) {
				im, err := pdfcpu.ExtractImage(context.Background(), ctx, &sd, false, "Im"+strconv.Itoa(n), n, false)
				if err != nil {
					return nil, err
				}
				if im == nil || im.Reader == nil || im.FileType == "" {
					return nil, nil
				}
				d, err := io.ReadAll(im)
				if err != nil {
					return nil, err
				}
				return &rendered{im.FileType, d}, nil
			})
			switch {
			case err != nil:
				fmt.Fprintf(&b, " extract err %s", classify(err))
			case img == nil:
				b.WriteString(" extract nil")
			default:
				h := hs(img.data)
				fmt.Fprintf(&b, " extract %s %s", img.typ, h)
				put(out, name, c.name, "i"+strconv.Itoa(n), img.typ+".out", []byte(h+"\n"))
			}
			fmt.Fprintf(&b, " cap %d\n", cap(sd.Content))
		}
		return b.Bytes(), nil
	})
}

// ---------------------------------------------------------------------------------------------
// build

// The inputs of the text helpers; the Rust oracle has the same lists.
var (
	buildDates   = []string{"D:20240315123045+02'00'", "D:20240315", "20240315123045Z", "D:2024", "D:20241301000000Z", "not a date", "D:20240229235959-08'00", "D:19991231235959.5Z", "2024-03-15 12:30", ""}
	buildLits    = []string{"abc", `a\(b\)c`, `\376\377\000H\000i`, `\357\273\277\303\244`, `\344\366`, `\376\377\000`}
	buildHexes   = []string{"48656C6C6F", "FEFF00480069", "EFBBBFC3A4", "4G", "", "4"}
	buildEscapes = []string{"plain", "Wert ü", "日本語", `a(b)c\`, "", "\xff"}
)

// hx is the hex text of b.
func hx(b []byte) string { return hex.EncodeToString(b) }

// build runs the entry points that byblos uses to make a document (CreateXRefTableWithRootDict,
// CreateContext, InsertObject, IndRefForNewObject, NewStreamDictForBuf, Catalog, Pages, the
// Dereference and FindTableEntry calls, VersionString, WriteContext) and the text helpers of
// types. It needs no input; it writes `_build.g.txt` with the sections build, write (the hs of the
// document's bytes) and text.
func build(out string) {
	const name, c = "_build", "g"
	result(out, name, c, "build", "out", func() ([]byte, error) {
		var b bytes.Buffer
		t, err := pdfcpu.CreateXRefTableWithRootDict()
		if err != nil {
			return nil, err
		}
		fmt.Fprintf(&b, "create ver %s size %s root %s\n", t.VersionString(), ptrInt(t.Size), refStr(t.Root))

		pages := types.NewDict()
		pages.InsertName("Type", "Pages")
		pages.InsertInt("Count", 1)
		pages.Insert("Kids", types.Array{})
		pagesRef, err := t.IndRefForNewObject(pages)
		if err != nil {
			return nil, err
		}
		sd, err := t.NewStreamDictForBuf([]byte("0 0 m 100 100 l S\n"))
		if err != nil {
			return nil, err
		}
		if err := sd.Encode(); err != nil {
			return nil, err
		}
		contentRef, err := t.IndRefForNewObject(*sd)
		if err != nil {
			return nil, err
		}
		page := types.NewDict()
		page.InsertName("Type", "Page")
		page.Insert("Parent", *pagesRef)
		page.Insert("MediaBox", types.Array{types.Integer(0), types.Integer(0), types.Integer(100), types.Integer(100)})
		page.Insert("Resources", types.NewDict())
		page.Insert("Contents", *contentRef)
		pageRef, err := t.IndRefForNewObject(page)
		if err != nil {
			return nil, err
		}
		pages["Kids"] = types.Array{*pageRef}
		root, err := t.Catalog()
		if err != nil {
			return nil, err
		}
		root.Insert("Pages", *pagesRef)
		objNr, err := t.InsertObject(types.Integer(7))
		if err != nil {
			return nil, err
		}
		fmt.Fprintf(&b, "inserted %d size %s\n", objNr, ptrInt(t.Size))

		rect := types.RectForDim(595.5, 842)
		fmt.Fprintf(&b, "rect %s\n", rectStr(rect))

		conf := newConf(model.DefaultResourceLimits())
		ctx := pdfcpu.CreateContext(t, conf)
		cat, err := t.Catalog()
		if err != nil {
			return nil, err
		}
		fmt.Fprintf(&b, "catalog %s\n", cat.PDFString())
		pr, err := t.Pages()
		if err != nil {
			return nil, err
		}
		fmt.Fprintf(&b, "pages %s\n", refStr(pr))
		for _, n := range []int{0, 1, 3, objNr, 99} {
			e, ok := t.FindTableEntry(n, 0)
			fmt.Fprintf(&b, "entry %d found %t", n, ok)
			if ok {
				fmt.Fprintf(&b, " gen %s free %t refcount %d", ptrInt(e.Generation), e.Free, e.RefCount)
			}
			b.WriteByte('\n')
		}
		_, ok := t.FindTableEntryForIndRef(pageRef)
		fmt.Fprintf(&b, "entry page found %t\n", ok)
		mb, err := t.DereferenceArray(page["MediaBox"])
		if err != nil {
			return nil, err
		}
		fmt.Fprintf(&b, "mediabox %s\n", mb.PDFString())
		o, err := t.Dereference(*pageRef)
		if err != nil {
			return nil, err
		}
		fmt.Fprintf(&b, "page %s\n", o.PDFString())
		d, err := t.DereferenceDict(*pagesRef)
		if err != nil {
			return nil, err
		}
		fmt.Fprintf(&b, "pagesdict %s\n", d.PDFString())
		csd, _, err := t.DereferenceStreamDict(*contentRef)
		if err != nil {
			return nil, err
		}
		fmt.Fprintf(&b, "content %s raw %s length %s\n", csd.PDFString(), hs(csd.Raw), ptrInt(csd.IntEntry("Length")))

		bb, err := writeCtx(ctx)
		if err != nil {
			return nil, err
		}
		put(out, name, c, "write", "out", []byte(hs(bb)+"\n"))
		fmt.Fprintf(&b, "write %s\n", hs(bb))
		return b.Bytes(), nil
	})
	result(out, name, c, "text", "out", func() ([]byte, error) {
		var b bytes.Buffer
		for _, s := range buildDates {
			tm, ok := types.DateTime(s, true)
			if !ok {
				fmt.Fprintf(&b, "date %s bad\n", hx([]byte(s)))
				continue
			}
			fmt.Fprintf(&b, "date %s %s\n", hx([]byte(s)), types.DateString(tm))
		}
		for _, s := range buildLits {
			r, err := types.StringLiteralToString(types.StringLiteral(s))
			fmt.Fprintf(&b, "lit %s %s\n", hx([]byte(s)), textOrErr(r, err))
		}
		for _, s := range buildHexes {
			r, err := types.HexLiteralToString(types.HexLiteral(s))
			fmt.Fprintf(&b, "hex %s %s\n", hx([]byte(s)), textOrErr(r, err))
		}
		for _, s := range buildEscapes {
			r, err := types.EscapedUTF16String(s)
			if err != nil {
				fmt.Fprintf(&b, "escape %s err %s\n", hx([]byte(s)), classify(err))
				continue
			}
			fmt.Fprintf(&b, "escape %s ok %s\n", hx([]byte(s)), hx([]byte(*r)))
		}
		return b.Bytes(), nil
	})
	flush(out, name, c)
}

// textOrErr is `ok <hex>` or `err <class>`.
func textOrErr(s string, err error) string {
	if err != nil {
		return "err " + classify(err)
	}
	return "ok " + hx([]byte(s))
}
