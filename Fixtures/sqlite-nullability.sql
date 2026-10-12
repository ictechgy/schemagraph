-- SQLite의 저장 의미론과 수집기별 PK nullability를 대조한다.
CREATE TABLE nullable_text(id TEXT PRIMARY KEY);
CREATE TABLE nullable_composite(a TEXT, b INTEGER, PRIMARY KEY(a, b));
CREATE TABLE nullable_int(id INT PRIMARY KEY);
CREATE TABLE nullable_desc(id INTEGER PRIMARY KEY DESC);
CREATE TABLE required_integer(id INTEGER PRIMARY KEY);
CREATE TABLE required_table_desc(id INTEGER, PRIMARY KEY(id DESC));
CREATE TABLE required_strict(id TEXT PRIMARY KEY) STRICT;
CREATE TABLE required_without_rowid(a TEXT, b INTEGER, PRIMARY KEY(a, b)) WITHOUT ROWID;
CREATE TABLE required_explicit(id TEXT PRIMARY KEY NOT NULL);
