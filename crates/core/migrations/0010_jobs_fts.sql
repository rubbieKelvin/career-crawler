-- Full-text index over jobs (milestone 12, brainstorms/10-llm.md). A natural-language search
-- matches its keywords here instead of with LIKE. External content: the index stores no copy,
-- it reads the row from `jobs` by rowid, and these triggers keep it in step. `rebuild` seeds it
-- with the jobs already stored; the title outweighs the description, skills sit in between.
CREATE VIRTUAL TABLE jobs_fts USING fts5(
  title, description, skills,
  content = 'jobs',
  content_rowid = 'id',
  tokenize = 'unicode61 remove_diacritics 2'
);
INSERT INTO jobs_fts(jobs_fts) VALUES ('rebuild');

CREATE TRIGGER jobs_fts_ai AFTER INSERT ON jobs BEGIN
  INSERT INTO jobs_fts(rowid, title, description, skills)
  VALUES (new.id, new.title, new.description, new.skills);
END;

CREATE TRIGGER jobs_fts_ad AFTER DELETE ON jobs BEGIN
  INSERT INTO jobs_fts(jobs_fts, rowid, title, description, skills)
  VALUES ('delete', old.id, old.title, old.description, old.skills);
END;

-- Only the indexed columns; the enricher updates the same row many times.
CREATE TRIGGER jobs_fts_au AFTER UPDATE OF title, description, skills ON jobs BEGIN
  INSERT INTO jobs_fts(jobs_fts, rowid, title, description, skills)
  VALUES ('delete', old.id, old.title, old.description, old.skills);
  INSERT INTO jobs_fts(rowid, title, description, skills)
  VALUES (new.id, new.title, new.description, new.skills);
END;
