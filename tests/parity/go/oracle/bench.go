package main

// -genbench and -bench (design section 14, docs/benchmarks.md). `-genbench <data dir> <bench dir>`
// writes the benchmark inputs, each named `<class>__<file>`: files of the corpus, and a few
// generated ones (synthetic images, large synthetic JPEGs, encrypted documents, CMYK JPEGs lifted out of the corpus).
// `-bench <n> <bench dir>` times the operation of each class on every file, n runs each, and
// prints `<name> TAB <bytes> TAB <ns per run>` (the same lines as `examples/bench.rs`). The
// bench does not set the determinism hooks: it measures what byblos gets.
//
// The classes and their operations (the setup of an operation is not timed):
//
//	read-*          ReadContext; bytes = input size
//	validate        api.Validate; bytes = input size
//	rvo             ReadValidateAndOptimize; bytes = input size
//	write-fallback  ReadContext and WriteContext; bytes = input size
//	decode-flate    DecodeWithLimit of every stream of a context read beforehand; bytes = decoded
//	encode-flate    NewStreamDictForBuf and Encode of a text file; bytes = encoded
//	image-*         ExtractImage of every image of a context read beforehand; bytes = image files

import (
	"bytes"
	"compress/zlib"
	"context"
	"fmt"
	"image"
	"image/jpeg"
	"io"
	"math"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"time"

	"github.com/dobbo-ca/pdf-go/pkg/api"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu/model"
	"github.com/dobbo-ca/pdf-go/pkg/pdfcpu/types"
)

// benchDocs are the files of the validate and rvo classes: the files of the three read classes.
var benchDocs = []string{
	"test.pdf", "empty.pdf", "testRot.pdf",
	"CenterOfWhy.pdf", "adobe_supplement_iso32000_1.pdf",
	"WaldenFull.pdf", "FOSDEM14_HPC_devroom_14_GoCUDA.pdf", "gobook.0.pdf",
}

// benchSets are the classes made of corpus files, by path under the data directory.
var benchSets = []struct {
	class string
	files []string
}{
	{"read-small", []string{"test.pdf", "empty.pdf", "testRot.pdf"}},
	{"read-objstm", []string{"CenterOfWhy.pdf", "adobe_supplement_iso32000_1.pdf"}},
	{"read-large", []string{"WaldenFull.pdf", "FOSDEM14_HPC_devroom_14_GoCUDA.pdf", "gobook.0.pdf"}},
	{"validate", benchDocs},
	{"rvo", benchDocs},
	{"decode-flate", []string{"WaldenFull.pdf", "gobook.0.pdf"}},
	{"encode-flate", []string{"filter/Mark.Twain-Tom.Sawyer.txt", "filter/pi.txt", "filter/e.txt"}},
	{"image-png", []string{"testImage.pdf"}},
	{"image-ccitt", []string{"T6.pdf", "HL1396.pdf"}},
}

// imagePDF is a one-page document whose image XObject (object 4) is raw, Flate-compressed.
func imagePDF(w, h int, cs string, raw []byte) []byte {
	var z bytes.Buffer
	zw := zlib.NewWriter(&z)
	_, _ = zw.Write(raw)
	_ = zw.Close()
	return buildPDF("", fmt.Sprintf("/Type /XObject /Subtype /Image /Width %d /Height %d /ColorSpace %s /BitsPerComponent 8 /Filter /FlateDecode", w, h, cs), z.Bytes())
}

// pixels is w*h*n bytes of a smooth pattern with some structure, so Flate has work to do.
func pixels(w, h, n int) []byte {
	b := make([]byte, 0, w*h*n)
	for y := 0; y < h; y++ {
		for x := 0; x < w; x++ {
			for c := 0; c < n; c++ {
				b = append(b, byte(x*3+y*5+c*50+(x*y)>>6))
			}
		}
	}
	return b
}

// dctPDF is a one-page document whose image XObject (object 4) is the JPEG data jpg.
func dctPDF(w, h int, cs string, jpg []byte) []byte {
	return buildPDF("", fmt.Sprintf("/Type /XObject /Subtype /Image /Width %d /Height %d /ColorSpace %s /BitsPerComponent 8 /Filter /DCTDecode", w, h, cs), jpg)
}

// jpegOf encodes the pixels (n = 1: gray, n = 3: RGB as YCbCr) with image/jpeg at quality 85.
func jpegOf(w, h, n int) []byte {
	px := pixels(w, h, n)
	var img image.Image
	if n == 1 {
		g := image.NewGray(image.Rect(0, 0, w, h))
		copy(g.Pix, px)
		img = g
	} else {
		r := image.NewRGBA(image.Rect(0, 0, w, h))
		for i := 0; i < w*h; i++ {
			copy(r.Pix[4*i:], []byte{px[3*i], px[3*i+1], px[3*i+2], 255})
		}
		img = r
	}
	var out bytes.Buffer
	if err := jpeg.Encode(&out, img, &jpeg.Options{Quality: 85}); err != nil {
		fatalf("%v", err)
	}
	return out.Bytes()
}

// Annex K luminance Huffman tables (counts per code length, then values).
var (
	dcCounts = [16]byte{0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0}
	dcVals   = []byte{0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11}
	acCounts = [16]byte{0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d}
	acVals   = []byte{
		0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
		0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
		0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
		0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
		0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
		0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
		0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
		0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
		0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
		0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
		0xf9, 0xfa,
	}
	zigzag = [64]int{
		0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28,
		35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
	}
)

// huffCodes returns the code and length of each symbol of a table.
func huffCodes(counts [16]byte, vals []byte) (code [256]uint32, size [256]uint) {
	c, k := uint32(0), 0
	for l := 1; l <= 16; l++ {
		for i := 0; i < int(counts[l-1]); i++ {
			code[vals[k]], size[vals[k]] = c, uint(l)
			c++
			k++
		}
		c <<= 1
	}
	return
}

// jpegCMYK encodes w*h CMYK pixels (w and h multiples of 8) as a baseline JPEG: four components
// of 1x1 sampling, one scan, the Annex K luminance tables, quantizer 8, an Adobe marker with
// transform 0 (image/jpeg has no CMYK encoder).
func jpegCMYK(w, h int, px []byte) []byte {
	var o bytes.Buffer
	seg := func(m byte, body []byte) {
		o.Write([]byte{0xff, m, byte((len(body) + 2) >> 8), byte(len(body) + 2)})
		o.Write(body)
	}
	o.Write([]byte{0xff, 0xd8})
	seg(0xee, []byte{'A', 'd', 'o', 'b', 'e', 0, 100, 0, 0, 0, 0, 0})
	dqt := []byte{0}
	for i := 0; i < 64; i++ {
		dqt = append(dqt, 8)
	}
	seg(0xdb, dqt)
	sof := []byte{8, byte(h >> 8), byte(h), byte(w >> 8), byte(w), 4}
	for c := byte(1); c <= 4; c++ {
		sof = append(sof, c, 0x11, 0)
	}
	seg(0xc0, sof)
	seg(0xc4, append(append([]byte{0x00}, dcCounts[:]...), dcVals...))
	seg(0xc4, append(append([]byte{0x10}, acCounts[:]...), acVals...))
	seg(0xda, []byte{4, 1, 0, 2, 0, 3, 0, 4, 0, 0, 63, 0})

	dcCode, dcSize := huffCodes(dcCounts, dcVals)
	acCode, acSize := huffCodes(acCounts, acVals)
	var acc uint32
	var nacc uint
	put := func(code uint32, n uint) {
		acc, nacc = acc<<n|code&(1<<n-1), nacc+n
		for nacc >= 8 {
			b := byte(acc >> (nacc - 8))
			o.WriteByte(b)
			if b == 0xff {
				o.WriteByte(0)
			}
			nacc -= 8
		}
	}
	// value puts the category code of v, then its bits.
	value := func(v int, code [256]uint32, size [256]uint, run int) {
		a := v
		if a < 0 {
			a = -a
		}
		cat := 0
		for a>>cat != 0 {
			cat++
		}
		sym := run<<4 | cat
		put(code[sym], size[sym])
		if cat > 0 {
			bits := v
			if v < 0 {
				bits = v - 1
			}
			put(uint32(bits), uint(cat))
		}
	}
	var cos [8][8]float64
	for u := 0; u < 8; u++ {
		for x := 0; x < 8; x++ {
			cos[u][x] = math.Cos(float64(2*x+1) * float64(u) * math.Pi / 16)
		}
	}
	var pred [4]int
	for by := 0; by < h; by += 8 {
		for bx := 0; bx < w; bx += 8 {
			for c := 0; c < 4; c++ {
				var blk, tmp [64]float64
				for y := 0; y < 8; y++ {
					for x := 0; x < 8; x++ {
						blk[y*8+x] = float64(px[((by+y)*w+bx+x)*4+c]) - 128
					}
				}
				for y := 0; y < 8; y++ { // rows
					for u := 0; u < 8; u++ {
						sum := 0.0
						for x := 0; x < 8; x++ {
							sum += blk[y*8+x] * cos[u][x]
						}
						tmp[y*8+u] = sum
					}
				}
				var q [64]int
				for u := 0; u < 8; u++ { // columns
					for v := 0; v < 8; v++ {
						sum := 0.0
						for y := 0; y < 8; y++ {
							sum += tmp[y*8+u] * cos[v][y]
						}
						cu, cv := 1.0, 1.0
						if u == 0 {
							cu = math.Sqrt2 / 2
						}
						if v == 0 {
							cv = math.Sqrt2 / 2
						}
						q[v*8+u] = int(math.Round(sum * cu * cv / 4 / 8))
					}
				}
				value(q[0]-pred[c], dcCode, dcSize, 0)
				pred[c] = q[0]
				run := 0
				for k := 1; k < 64; k++ {
					v := q[zigzag[k]]
					if v == 0 {
						run++
						continue
					}
					for ; run > 15; run -= 16 {
						put(acCode[0xf0], acSize[0xf0])
					}
					value(v, acCode, acSize, run)
					run = 0
				}
				if run > 0 {
					put(acCode[0], acSize[0])
				}
			}
		}
	}
	if nacc > 0 {
		put(1<<(8-nacc)-1, 8-nacc)
	}
	o.Write([]byte{0xff, 0xd9})
	return o.Bytes()
}

// genBench writes the benchmark inputs.
func genBench(dataDir, outDir string) {
	if err := os.MkdirAll(outDir, 0o755); err != nil {
		fatalf("%v", err)
	}
	emit := func(class, name string, data []byte) {
		p := filepath.Join(outDir, class+"__"+strings.ReplaceAll(name, "/", "_"))
		if err := os.WriteFile(p, data, 0o644); err != nil {
			fatalf("%v", err)
		}
	}
	for _, s := range benchSets {
		for _, f := range s.files {
			data, err := os.ReadFile(filepath.Join(dataDir, f))
			if err != nil {
				fatalf("%v", err)
			}
			emit(s.class, f, data)
		}
	}

	// Synthetic images: gray and RGB for the PNG renderers, CMYK for the TIFF one.
	emit("image-png", "synth-gray.pdf", imagePDF(1024, 1024, "/DeviceGray", pixels(1024, 1024, 1)))
	emit("image-png", "synth-rgb.pdf", imagePDF(1024, 768, "/DeviceRGB", pixels(1024, 768, 3)))
	emit("image-tiff", "synth-cmyk.pdf", imagePDF(512, 512, "/DeviceCMYK", pixels(512, 512, 4)))

	// Large DCT images (1024x1024; the corpus ones are 0.08 Mpx and baseline only): image/jpeg
	// gray and YCbCr, and a CMYK made by jpegCMYK. The CMYK one joins the corpus ones in its class.
	emit("image-dct-gray", "synth-gray.pdf", dctPDF(1024, 1024, "/DeviceGray", jpegOf(1024, 1024, 1)))
	emit("image-dct-rgb", "synth-rgb.pdf", dctPDF(1024, 1024, "/DeviceRGB", jpegOf(1024, 1024, 3)))
	emit("image-dct-cmyk", "synth-cmyk-1024.pdf", dctPDF(1024, 1024, "/DeviceCMYK", jpegCMYK(1024, 1024, pixels(1024, 1024, 4))))

	// An encrypted copy of a larger document, under AES-256 and under RC4-128, for the write
	// fallback (a document that is read, decrypted, and written again encrypted).
	base, err := os.ReadFile(filepath.Join(dataDir, "CenterOfWhy.pdf"))
	if err != nil {
		fatalf("%v", err)
	}
	for _, a := range []struct {
		tag  string
		aes  bool
		bits int
	}{{"aes-256", true, 256}, {"rc4-128", false, 128}} {
		conf := model.NewStatelessConfiguration()
		conf.UserPW, conf.OwnerPW = "", "owner"
		conf.EncryptUsingAES, conf.EncryptKeyLength = a.aes, a.bits
		var out bytes.Buffer
		if _, err := protect(func() (struct{}, error) {
			return struct{}{}, api.Encrypt(context.Background(), bytes.NewReader(base), &out, conf)
		}); err != nil {
			fmt.Fprintf(os.Stderr, "genbench: no %s file: %v\n", a.tag, err)
			continue
		}
		emit("write-fallback", a.tag+".pdf", out.Bytes())
	}

	// CMYK DCT images: the first two that the corpus holds, each in a document of its own.
	found := 0
	_ = filepath.WalkDir(dataDir, func(p string, d os.DirEntry, err error) error {
		if err != nil || d.IsDir() || found >= 2 || !strings.HasSuffix(strings.ToLower(p), ".pdf") {
			return nil
		}
		data, err := os.ReadFile(p)
		if err != nil || len(data) > 4<<20 {
			return nil
		}
		ctx, err := readCtx(data, benchConf())
		if err != nil {
			return nil
		}
		t := ctx.XRefTable
		for _, n := range numbers(t) {
			e := t.Table[n]
			if e == nil || found >= 2 {
				continue
			}
			sd, ok := e.Object.(types.StreamDict)
			if !ok || len(sd.FilterPipeline) != 1 || sd.FilterPipeline[0].Name != "DCTDecode" || len(sd.Raw) == 0 {
				continue
			}
			cs, w, h := sd.NameEntry("ColorSpace"), sd.IntEntry("Width"), sd.IntEntry("Height")
			if st := sd.Subtype(); st == nil || *st != "Image" || cs == nil || *cs != "DeviceCMYK" || w == nil || h == nil {
				continue
			}
			found++
			dict := fmt.Sprintf("/Type /XObject /Subtype /Image /Width %d /Height %d /ColorSpace /DeviceCMYK /BitsPerComponent 8 /Filter /DCTDecode", *w, *h)
			emit("image-dct-cmyk", "cmyk"+strconv.Itoa(found)+".pdf", buildPDF("", dict, sd.Raw))
		}
		return nil
	})
	if found == 0 {
		fmt.Fprintln(os.Stderr, "genbench: no CMYK DCT image in the corpus; the class image-dct-cmyk is empty")
	}
}

func benchConf() *model.Configuration { return newConf(model.DefaultResourceLimits()) }

// A bench operation returns the bytes it counts and the time of the part that is timed.
type benchFunc func(data []byte) (int64, time.Duration, error)

func benchOp(class string) benchFunc {
	switch class {
	case "read-small", "read-objstm", "read-large":
		return benchRead
	case "validate":
		return benchValidate
	case "rvo":
		return benchRvo
	case "write-fallback":
		return benchWrite
	case "decode-flate":
		return benchDecode
	case "encode-flate":
		return benchEncode
	case "image-png", "image-tiff", "image-dct-gray", "image-dct-rgb", "image-dct-cmyk", "image-ccitt":
		return benchImages
	}
	return nil
}

func benchRead(data []byte) (int64, time.Duration, error) {
	conf := benchConf()
	t0 := time.Now()
	_, err := readCtx(data, conf)
	return int64(len(data)), time.Since(t0), err
}

func benchValidate(data []byte) (int64, time.Duration, error) {
	conf := benchConf()
	t0 := time.Now()
	_, err := protect(func() (struct{}, error) {
		return struct{}{}, api.Validate(context.Background(), bytes.NewReader(data), conf, nil)
	})
	return int64(len(data)), time.Since(t0), err
}

func benchRvo(data []byte) (int64, time.Duration, error) {
	conf := benchConf()
	t0 := time.Now()
	_, err := protect(func() (*model.Context, error) { return rvo(data, conf) })
	return int64(len(data)), time.Since(t0), err
}

func benchWrite(data []byte) (int64, time.Duration, error) {
	conf := benchConf()
	t0 := time.Now()
	_, err := protect(func() ([]byte, error) {
		ctx, err := api.ReadContext(context.Background(), bytes.NewReader(data), conf)
		if err != nil {
			return nil, err
		}
		return writeCtx(ctx)
	})
	return int64(len(data)), time.Since(t0), err
}

func benchDecode(data []byte) (int64, time.Duration, error) {
	conf := benchConf()
	ctx, err := readCtx(data, conf)
	if err != nil {
		return 0, 0, err
	}
	t := ctx.XRefTable
	var total int64
	t0 := time.Now()
	for _, n := range streamsOf(t, 1<<30) {
		sd := fresh(t, n)
		if _, err := protect(func() (struct{}, error) {
			return struct{}{}, sd.DecodeWithLimit(conf.DecodeLimit())
		}); err == nil {
			total += int64(len(sd.Content))
		}
	}
	return total, time.Since(t0), nil
}

func benchEncode(data []byte) (int64, time.Duration, error) {
	var n int64
	t0 := time.Now()
	_, err := protect(func() (struct{}, error) {
		nsd, err := (&model.XRefTable{}).NewStreamDictForBuf(data)
		if err != nil {
			return struct{}{}, err
		}
		if err := nsd.Encode(); err != nil {
			return struct{}{}, err
		}
		n = int64(len(nsd.Raw))
		return struct{}{}, nil
	})
	return n, time.Since(t0), err
}

func benchImages(data []byte) (int64, time.Duration, error) {
	ctx, err := readCtx(data, benchConf())
	if err != nil {
		return 0, 0, err
	}
	t := ctx.XRefTable
	var total int64
	count := 0
	t0 := time.Now()
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
		d, err := protect(func() ([]byte, error) {
			im, err := pdfcpu.ExtractImage(context.Background(), ctx, &sd, false, "Im"+strconv.Itoa(n), n, false)
			if err != nil || im == nil || im.Reader == nil {
				return nil, err
			}
			return io.ReadAll(im)
		})
		if err == nil {
			total += int64(len(d))
		}
	}
	return total, time.Since(t0), nil
}

// bench times every file of dir, n runs each, and prints a line per file.
func bench(n int, dir string) {
	ents, err := os.ReadDir(dir)
	if err != nil {
		fatalf("%v", err)
	}
	for _, e := range ents {
		name := e.Name()
		class, _, _ := strings.Cut(name, "__")
		op := benchOp(class)
		if e.IsDir() || op == nil {
			continue
		}
		data, err := os.ReadFile(filepath.Join(dir, name))
		if err != nil {
			fatalf("%v", err)
		}
		if _, _, err := op(data); err != nil {
			fmt.Fprintf(os.Stderr, "bench: skip %s: %v\n", name, err)
			continue
		}
		var size int64
		var total time.Duration
		for i := 0; i < n; i++ {
			runtime.GC()
			s, d, err := op(data)
			if err != nil {
				fatalf("%s: %v", name, err)
			}
			size, total = s, total+d
		}
		fmt.Printf("%s\t%d\t%d\n", name, size, total.Nanoseconds()/int64(n))
	}
}
