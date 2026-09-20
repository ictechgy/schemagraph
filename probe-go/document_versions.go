package main

import (
	"encoding/json"
	"fmt"
	"strings"
)

// validateDocumentVersion은 송신자가 엔진이 정의한 문서 envelope만 만들도록
// 버전을 먼저 제한한다. 알 수 없는 버전은 필드를 추측해 내보내지 않는다.
func validateDocumentVersion(version int) error {
	if version != 1 && version != 2 {
		return fmt.Errorf("unsupported document version %d; choose 1 or 2", version)
	}
	return nil
}

type documentProducer struct {
	Name string `json:"name"`
}

type documentV2 struct {
	Version          int                 `json:"version"`
	Dialect          string              `json:"dialect"`
	Producer         documentProducer    `json:"producer"`
	RequiredFeatures []string            `json:"required_features"`
	Schemas          []SchemaDoc         `json:"schemas"`
	Limitations      []string            `json:"limitations"`
	Context          *CollectionContext  `json:"context,omitempty"`
	Dependencies     []CatalogDependency `json:"dependencies,omitempty"`
}

type ndjsonDocumentV1 struct {
	Type        string             `json:"type"`
	Version     int                `json:"version"`
	Dialect     string             `json:"dialect"`
	Reader      string             `json:"reader"`
	Limitations []string           `json:"limitations"`
	Context     *CollectionContext `json:"context,omitempty"`
}

type ndjsonDocumentV2 struct {
	Type             string             `json:"type"`
	Version          int                `json:"version"`
	Dialect          string             `json:"dialect"`
	Producer         documentProducer   `json:"producer"`
	RequiredFeatures []string           `json:"required_features"`
	Limitations      []string           `json:"limitations"`
	Context          *CollectionContext `json:"context,omitempty"`
}

// encodeDocument은 JSON과 NDJSON 모두에 같은 v1/v2 envelope 규칙을 적용한다.
// 레코드 data는 typed 구조체로 직렬화해 큰 int64 usage 카운터를 보존한다.
func encodeDocument(doc CatalogDocument, version int, format string) ([]byte, error) {
	if err := validateDocumentVersion(version); err != nil {
		return nil, err
	}
	if version == 2 && doc.Reader == "" {
		return nil, fmt.Errorf("catalog v2 requires a nonempty producer name")
	}
	switch format {
	case "json":
		return encodeDocumentJSON(doc, version)
	case "ndjson":
		return encodeDocumentNDJSON(doc, version)
	default:
		return nil, fmt.Errorf("unsupported document format %q; choose json or ndjson", format)
	}
}

func encodeDocumentJSON(doc CatalogDocument, version int) ([]byte, error) {
	var value any
	if version == 1 {
		v1 := doc
		v1.Version = version
		value = v1
	} else {
		value = documentV2{
			Version: version, Dialect: doc.Dialect,
			Producer:         documentProducer{Name: doc.Reader},
			RequiredFeatures: documentFeatures(doc),
			Schemas:          doc.Schemas, Limitations: doc.Limitations,
			Context: doc.Context, Dependencies: doc.Dependencies,
		}
	}
	encoded, err := json.MarshalIndent(value, "", "  ")
	if err != nil {
		return nil, err
	}
	return append(encoded, '\n'), nil
}

func encodeDocumentNDJSON(doc CatalogDocument, version int) ([]byte, error) {
	var b strings.Builder
	line := func(value any) error {
		encoded, err := json.Marshal(value)
		if err != nil {
			return err
		}
		b.Write(encoded)
		b.WriteByte('\n')
		return nil
	}

	if version == 1 {
		if err := line(ndjsonDocumentV1{
			Type: "document", Version: version, Dialect: doc.Dialect,
			Reader: doc.Reader, Limitations: []string{},
			Context: doc.Context,
		}); err != nil {
			return nil, err
		}
	} else if err := line(ndjsonDocumentV2{
		Type: "document", Version: version, Dialect: doc.Dialect,
		Producer:         documentProducer{Name: doc.Reader},
		RequiredFeatures: documentFeatures(doc), Limitations: []string{},
		Context: doc.Context,
	}); err != nil {
		return nil, err
	}

	for _, schema := range doc.Schemas {
		if err := line(map[string]any{"type": "schema", "name": schema.Name}); err != nil {
			return nil, err
		}
		for _, object := range schema.Objects {
			if err := line(map[string]any{
				"type": "object", "schema": schema.Name, "data": object,
			}); err != nil {
				return nil, err
			}
		}
		for _, routine := range schema.Routines {
			if err := line(map[string]any{
				"type": "routine", "schema": schema.Name, "data": routine,
			}); err != nil {
				return nil, err
			}
		}
	}
	for _, dependency := range doc.Dependencies {
		if err := line(map[string]any{"type": "dependency", "data": dependency}); err != nil {
			return nil, err
		}
	}
	if err := line(map[string]any{"type": "limitations", "data": doc.Limitations}); err != nil {
		return nil, err
	}
	return []byte(b.String()), nil
}

func documentFeatures(doc CatalogDocument) []string {
	usage, members := false, false
	for _, schema := range doc.Schemas {
		for _, object := range schema.Objects {
			usage = usage || object.Usage != nil
			for _, index := range object.Indexes {
				usage = usage || index.Usage != nil
			}
		}
		for _, routine := range schema.Routines {
			usage = usage || routine.Usage != nil
			members = members || routine.MemberOf != nil
		}
	}
	features := make([]string, 0, 2)
	if len(doc.Dependencies) > 0 {
		features = append(features, "catalog-dependencies-v1")
	}
	if members {
		features = append(features, "package-members-v1")
	}
	if usage {
		features = append(features, "usage-v1")
	}
	return features
}
