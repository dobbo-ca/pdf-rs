module pdf-parity-oracle

go 1.26.0

require (
	github.com/clipperhouse/uax29/v2 v2.7.0
	github.com/dobbo-ca/pdf-go v0.0.0
	github.com/hhrutter/tiff v1.0.7
	github.com/mattn/go-runewidth v0.0.31
	github.com/xdg-go/stringprep v1.0.4
	go.yaml.in/yaml/v3 v3.0.5
	golang.org/x/crypto v0.58.0
	golang.org/x/image v0.47.0
	golang.org/x/text v0.43.0
)

replace github.com/dobbo-ca/pdf-go => ../upstream

replace github.com/clipperhouse/uax29/v2 => ../deps/uax29

replace github.com/hhrutter/tiff => ../deps/hhrutter-tiff

replace github.com/mattn/go-runewidth => ../deps/go-runewidth

replace github.com/xdg-go/stringprep => ../deps/stringprep

replace go.yaml.in/yaml/v3 => ../deps/yaml-v3

replace golang.org/x/crypto => ../deps/x-crypto

replace golang.org/x/image => ../deps/x-image

replace golang.org/x/text => ../deps/x-text
