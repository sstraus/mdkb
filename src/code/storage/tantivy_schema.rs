//! Tantivy schema definition for the code intelligence index.
//!
//! A single Tantivy index stores multiple document types (symbols,
//! relationships, files, metadata) distinguished by a `doc_type` field.

use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, Schema, SchemaBuilder, TextFieldIndexing,
    TextOptions, FAST, STORED, STRING,
};

/// All named fields in the Tantivy schema, grouped by document type.
#[derive(Debug)]
pub struct IndexSchema {
    // Document type discriminator: "symbol", "relationship", "file", "metadata"
    pub doc_type: Field,

    // --- Symbol fields ---
    pub symbol_id: Field,
    /// Exact match on full symbol name (STRING, no tokenization).
    pub name: Field,
    /// Ngram-tokenized name for partial/substring matching.
    pub name_text: Field,
    pub doc_comment: Field,
    pub signature: Field,
    pub module_path: Field,
    pub kind: Field,
    pub file_path: Field,
    pub line_number: Field,
    pub column: Field,
    pub end_line: Field,
    pub end_column: Field,
    pub context: Field,
    pub visibility: Field,
    pub scope_context: Field,
    pub language: Field,

    // --- Relationship fields ---
    pub from_symbol_id: Field,
    pub to_symbol_id: Field,
    pub relation_kind: Field,
    pub relation_weight: Field,
    pub relation_line: Field,
    pub relation_column: Field,
    pub relation_context: Field,

    // --- File info fields ---
    pub file_id: Field,
    pub file_hash: Field,
    pub file_timestamp: Field,
    pub file_mtime: Field,

    // --- Metadata fields (counters, index state) ---
    pub meta_key: Field,
    pub meta_value: Field,
}

impl IndexSchema {
    /// Build the Tantivy schema and return the (Schema, IndexSchema) pair.
    pub fn build() -> (Schema, Self) {
        let mut builder = SchemaBuilder::default();

        let doc_type = builder.add_text_field("doc_type", STRING | STORED | FAST);

        let indexed_u64 = NumericOptions::default()
            .set_indexed()
            .set_stored()
            .set_fast();

        // --- Symbol fields ---
        let symbol_id = builder.add_u64_field("symbol_id", indexed_u64.clone());
        let name = builder.add_text_field("name", STRING | STORED);

        let ngram_text = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("ngram")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored();
        let name_text = builder.add_text_field("name_text", ngram_text);

        let text_options = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("default")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored();

        let doc_comment = builder.add_text_field("doc_comment", text_options.clone());
        let signature = builder.add_text_field("signature", text_options.clone());
        let context = builder.add_text_field("context", text_options.clone());
        let module_path = builder.add_text_field("module_path", STRING | STORED);
        let kind = builder.add_text_field("kind", STRING | STORED);
        let file_path = builder.add_text_field("file_path", STRING | STORED);
        let line_number = builder.add_u64_field("line_number", indexed_u64.clone());
        let column = builder.add_u64_field("column", STORED);
        let end_line = builder.add_u64_field("end_line", STORED | FAST);
        let end_column = builder.add_u64_field("end_column", STORED);
        let visibility = builder.add_u64_field("visibility", STORED);
        let scope_context = builder.add_text_field("scope_context", STRING | STORED);
        let language = builder.add_text_field("language", STRING | STORED | FAST);

        // --- Relationship fields ---
        let from_symbol_id = builder.add_u64_field("from_symbol_id", indexed_u64.clone());
        let to_symbol_id = builder.add_u64_field("to_symbol_id", indexed_u64.clone());
        let relation_kind = builder.add_text_field("relation_kind", STRING | STORED | FAST);
        let relation_weight = builder.add_f64_field("relation_weight", STORED);
        let relation_line = builder.add_u64_field("relation_line", STORED);
        let relation_column = builder.add_u64_field("relation_column", STORED);
        let relation_context = builder.add_text_field("relation_context", text_options);

        // --- File info fields ---
        let file_id = builder.add_u64_field("file_id", indexed_u64);
        let file_hash = builder.add_text_field("file_hash", STRING | STORED);
        let file_timestamp = builder.add_u64_field("file_timestamp", STORED | FAST);
        let file_mtime = builder.add_u64_field("file_mtime", STORED | FAST);

        // --- Metadata fields ---
        let meta_key = builder.add_text_field("meta_key", STRING | STORED | FAST);
        let meta_value = builder.add_u64_field("meta_value", STORED | FAST);

        let schema = builder.build();
        let index_schema = Self {
            doc_type,
            symbol_id,
            name,
            name_text,
            doc_comment,
            signature,
            module_path,
            kind,
            file_path,
            line_number,
            column,
            end_line,
            end_column,
            context,
            visibility,
            scope_context,
            language,
            from_symbol_id,
            to_symbol_id,
            relation_kind,
            relation_weight,
            relation_line,
            relation_column,
            relation_context,
            file_id,
            file_hash,
            file_timestamp,
            file_mtime,
            meta_key,
            meta_value,
        };

        (schema, index_schema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_builds_successfully() {
        let (schema, _index_schema) = IndexSchema::build();
        // Verify key fields exist
        assert!(schema.get_field("doc_type").is_ok());
        assert!(schema.get_field("symbol_id").is_ok());
        assert!(schema.get_field("name").is_ok());
        assert!(schema.get_field("name_text").is_ok());
        assert!(schema.get_field("relation_kind").is_ok());
        assert!(schema.get_field("file_id").is_ok());
        assert!(schema.get_field("meta_key").is_ok());
    }

    #[test]
    fn test_schema_field_count() {
        let (schema, _) = IndexSchema::build();
        // 16 symbol + 7 relationship + 4 file + 2 metadata + 1 doc_type = 30
        let field_count = schema.fields().count();
        assert_eq!(field_count, 30);
    }

    #[test]
    fn test_schema_fields_match_struct() {
        let (schema, index_schema) = IndexSchema::build();
        // Verify each struct field resolves to the correct schema field
        assert_eq!(
            schema.get_field("doc_type").unwrap(),
            index_schema.doc_type
        );
        assert_eq!(
            schema.get_field("symbol_id").unwrap(),
            index_schema.symbol_id
        );
        assert_eq!(schema.get_field("name").unwrap(), index_schema.name);
        assert_eq!(
            schema.get_field("name_text").unwrap(),
            index_schema.name_text
        );
        assert_eq!(
            schema.get_field("from_symbol_id").unwrap(),
            index_schema.from_symbol_id
        );
        assert_eq!(
            schema.get_field("file_hash").unwrap(),
            index_schema.file_hash
        );
        assert_eq!(
            schema.get_field("meta_key").unwrap(),
            index_schema.meta_key
        );
    }
}
