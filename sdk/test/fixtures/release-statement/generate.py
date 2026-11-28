#!/usr/bin/env python3
"""Regenerate vectors.json: release statements and the answer every SDK must give.

Each vector is one edit, or a few edits at once, to the statement signed for the
published Development release. The statement check runs on a payload whose
signature has already verified, so the vectors carry payload bytes only.

Usage: python3 -I generate.py PUBLISHED_BUNDLE > vectors.json
"""
import base64
import json
import sys

ENVIRONMENT = "development"


def published_statement(bundle_path):
    with open(bundle_path, "rb") as bundle_file:
        bundle = json.load(bundle_file)
    return base64.b64decode(bundle["dsseEnvelope"]["payload"])


def replaced(text, old, new):
    """Replace exactly one occurrence, so a stale edit fails loudly."""
    if text.count(old) != 1:
        raise SystemExit(f"expected exactly one {old!r} in the statement")
    return text.replace(old, new)


def edited(statement, *edits):
    text = statement.decode("utf-8")
    for old, new in edits:
        text = replaced(text, old, new)
    return text.encode("utf-8")


# Named pieces of the published statement, so each vector reads as one edit.
STATEMENT_TYPE = '"_type": "https://in-toto.io/Statement/v1"'
PREDICATE_TYPE = '"predicateType": "https://maple.ai/attestations/enclave-release/v1"'
SUBJECT_DIGEST = '"sha256": "b84a3ce9a9a8ca19dcd060ab66d2e17b528154374c4f18a282f7065ff18aaad0"'
SUBJECT_NAME = '"name": "image.eif"'
ENVIRONMENT_MEMBER = '"environment": "development"'
PCR0_VALUE = "b422d5fcb25929a1b76f0eb8aa10c26a78c635a86a58039b48f22eb0dc4dc6f8ae6e646aa2fbbc57db756e6f06cad3dc"
PCR1_VALUE = "e83dc536d4cf82daf0e0821e7fdbddf25cd76090602cb67bef7c9d5ab2914417ee6a94bd0c9c1e7ed15a3c3c3bfcc0a3"
PCR2_VALUE = "57b7e86919f571beff7539cb8643baafe81ec9f2ffb3434d683268978675a664e0d043dead3367aba14a5d2814a4be97"
REPOSITORY = '"repository": "https://github.com/MaplePrivacyLabs/Maple"'
COMMIT = '"commit": "bcf6e84476be2cf08b575c5303f5a83d98adbf40"'
RUN_ID = '"runId": "44636648269"'
RUN_ATTEMPT = '"runAttempt": 1'
OTHER_PCR = "00" * 48

ACCEPT = "accept"

# (id, what it shows, edits, expected). An expected value other than ACCEPT is
# the failure category.
ONE_FAULT = [
    ("published", "the statement as signed", [], ACCEPT),
    ("statement-type-missing", "no _type member", [(STATEMENT_TYPE + ",", "")], "statement-unsupported"),
    ("statement-type-older", "an older statement type",
     [(STATEMENT_TYPE, '"_type": "https://in-toto.io/Statement/v0.1"')], "statement-unsupported"),
    ("subject-empty", "an empty subject list",
     [('"subject": [\n    {\n      ' + SUBJECT_NAME + ',\n      "digest": {\n        ' + SUBJECT_DIGEST
       + '\n      }\n    }\n  ]', '"subject": []')], "statement-malformed"),
    ("subject-name-empty", "a subject with an empty name", [(SUBJECT_NAME, '"name": ""')], "statement-malformed"),
    ("subject-digest-uppercase", "a subject digest in uppercase hex",
     [(SUBJECT_DIGEST, SUBJECT_DIGEST.upper().replace("SHA256", "sha256"))], "statement-malformed"),
    ("predicate-type-other", "another predicate type",
     [(PREDICATE_TYPE, '"predicateType": "https://example.com/other"')], "statement-unsupported"),
    ("predicate-not-object", "a predicate that is a string",
     [('"predicate": {', '"predicate": "release", "ignored": {')], "statement-malformed"),
    ("environment-production", "a production environment",
     [(ENVIRONMENT_MEMBER, '"environment": "production"')], "statement-environment-mismatch"),
    ("environment-missing", "no environment member", [(ENVIRONMENT_MEMBER + ",", "")],
     "statement-environment-mismatch"),
    ("pcr0-uppercase", "PCR0 in uppercase hex", [(PCR0_VALUE, PCR0_VALUE.upper())], "statement-malformed"),
    ("pcr1-short", "a PCR1 that is too short", [(PCR1_VALUE, PCR1_VALUE[:-2])], "statement-malformed"),
    ("pcr0-other", "a different PCR0", [(PCR0_VALUE, OTHER_PCR)], "statement-measurement-mismatch"),
    ("pcr1-other", "a different PCR1", [(PCR1_VALUE, OTHER_PCR)], "statement-measurement-mismatch"),
    ("pcr2-other", "a different PCR2", [(PCR2_VALUE, OTHER_PCR)], "statement-measurement-mismatch"),
    ("repository-empty", "an empty repository", [(REPOSITORY, '"repository": ""')], "statement-malformed"),
    ("repository-other", "another repository; it is audit data, not policy",
     [(REPOSITORY, '"repository": "https://github.com/example/renamed"')], ACCEPT),
    ("commit-short", "a shortened commit", [(COMMIT, '"commit": "bcf6e844"')], "statement-malformed"),
    ("commit-uppercase", "a commit in uppercase hex", [(COMMIT, COMMIT.upper().replace("COMMIT", "commit"))],
     "statement-malformed"),
    ("run-id-other", "another build run; it is audit data, not policy", [(RUN_ID, '"runId": "1"')], ACCEPT),
    ("run-id-not-digits", "a build run ID that is not a number", [(RUN_ID, '"runId": "abc"')],
     "statement-malformed"),
    ("run-id-number", "a build run ID written as a JSON number", [(RUN_ID, '"runId": 44636648269')],
     "statement-malformed"),
    ("run-attempt-two", "a second attempt", [(RUN_ATTEMPT, '"runAttempt": 2')], ACCEPT),
    ("run-attempt-decimal-point", "an attempt written as 1.0, which is the number 1",
     [(RUN_ATTEMPT, '"runAttempt": 1.0')], ACCEPT),
    ("run-attempt-exponent", "an attempt written as 1e0, which is the number 1",
     [(RUN_ATTEMPT, '"runAttempt": 1e0')], ACCEPT),
    ("run-attempt-zero", "an attempt of zero", [(RUN_ATTEMPT, '"runAttempt": 0')], "statement-malformed"),
    ("run-attempt-negative", "a negative attempt", [(RUN_ATTEMPT, '"runAttempt": -1')], "statement-malformed"),
    ("run-attempt-fraction", "an attempt of 1.5", [(RUN_ATTEMPT, '"runAttempt": 1.5')], "statement-malformed"),
    ("run-attempt-string", "an attempt written as a string", [(RUN_ATTEMPT, '"runAttempt": "1"')],
     "statement-malformed"),
    ("run-attempt-largest", "the largest attempt every JSON reader holds exactly",
     [(RUN_ATTEMPT, '"runAttempt": 9007199254740991')], ACCEPT),
    ("run-attempt-too-large", "an attempt too large to hold exactly",
     [(RUN_ATTEMPT, '"runAttempt": 9007199254740993')], "statement-malformed"),
    ("build-missing", "no build member",
     [(',\n    "build": {\n      ' + RUN_ID + ',\n      ' + RUN_ATTEMPT + '\n    }', "")], "statement-malformed"),
    ("member-repeated", "the environment named twice",
     [(ENVIRONMENT_MEMBER, '"environment": "production", "environment": "development"')], "duplicate-json-key"),
    ("member-repeated-escaped", "the environment named twice, once with an escape",
     [(ENVIRONMENT_MEMBER, '"environment": "production", "\\u0065nvironment": "development"')],
     "duplicate-json-key"),
    ("unknown-member", "an extra member the policy does not read",
     [(ENVIRONMENT_MEMBER, ENVIRONMENT_MEMBER + ', "note": "extra"')], ACCEPT),
]

# Several faults at once: the answer is the first check that fails, in the
# order README.md lists.
SEVERAL_FAULTS = [
    ("type-then-environment", "an older statement type and a production environment",
     [(STATEMENT_TYPE, '"_type": "https://in-toto.io/Statement/v0.1"'),
      (ENVIRONMENT_MEMBER, '"environment": "production"')], "statement-unsupported"),
    ("subject-then-predicate-type", "an empty subject name and another predicate type",
     [(SUBJECT_NAME, '"name": ""'), (PREDICATE_TYPE, '"predicateType": "https://example.com/other"')],
     "statement-malformed"),
    ("predicate-type-then-environment", "another predicate type and a production environment",
     [(PREDICATE_TYPE, '"predicateType": "https://example.com/other"'),
      (ENVIRONMENT_MEMBER, '"environment": "production"')], "statement-unsupported"),
    ("environment-then-pcr-shape", "a production environment and a malformed PCR0",
     [(ENVIRONMENT_MEMBER, '"environment": "production"'), (PCR0_VALUE, PCR0_VALUE.upper())],
     "statement-environment-mismatch"),
    ("environment-then-commit", "a production environment and a shortened commit",
     [(ENVIRONMENT_MEMBER, '"environment": "production"'), (COMMIT, '"commit": "bcf6e844"')],
     "statement-environment-mismatch"),
    ("pcr-shape-then-pcr-value", "a malformed PCR1 and a different PCR0",
     [(PCR1_VALUE, PCR1_VALUE[:-2]), (PCR0_VALUE, OTHER_PCR)], "statement-malformed"),
    ("pcr-value-then-commit", "a different PCR2 and a shortened commit",
     [(PCR2_VALUE, OTHER_PCR), (COMMIT, '"commit": "bcf6e844"')], "statement-measurement-mismatch"),
    ("pcr-value-then-build", "a different PCR0 and an attempt of zero",
     [(PCR0_VALUE, OTHER_PCR), (RUN_ATTEMPT, '"runAttempt": 0')], "statement-measurement-mismatch"),
    ("repeated-member-then-type", "the environment named twice and an older statement type",
     [(ENVIRONMENT_MEMBER, '"environment": "production", "environment": "development"'),
      (STATEMENT_TYPE, '"_type": "https://in-toto.io/Statement/v0.1"')], "duplicate-json-key"),
]

# Payloads that are not edits of the statement's text.
RAW = [
    ("not-utf8", "bytes that are not UTF-8", b"\xff\xfe{}", "statement-malformed"),
    ("not-json", "text that is not JSON", b"{", "json-syntax-error"),
    ("json-array", "a JSON array", b"[1,2,3]", "statement-malformed"),
    ("json-null", "JSON null", b"null", "statement-malformed"),
]

BYTE_ORDER_MARK = b"\xef\xbb\xbf"


def framed(statement):
    """Payloads built from the whole statement instead of one edit inside it."""
    repeated_member = edited(
        statement, (ENVIRONMENT_MEMBER, '"environment": "production", "environment": "development"'))
    return [
        ("byte-order-mark", "the statement after a UTF-8 byte order mark, which is not JSON",
         BYTE_ORDER_MARK + statement, "json-syntax-error"),
        ("repeated-member-then-syntax", "the environment named twice in text that is cut short",
         repeated_member.rstrip()[:-1], "json-syntax-error"),
    ]


def vector(vector_id, shows, payload, expected):
    entry = {"id": vector_id, "shows": shows, "payload": base64.b64encode(payload).decode("ascii")}
    if expected == ACCEPT:
        entry["expected"] = {"result": "accept"}
    else:
        entry["expected"] = {"result": "reject", "category": expected}
    return entry


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    statement = published_statement(sys.argv[1])
    vectors = []
    for vector_id, shows, edits, expected in ONE_FAULT + SEVERAL_FAULTS:
        vectors.append(vector(vector_id, shows, edited(statement, *edits), expected))
    for vector_id, shows, payload, expected in RAW + framed(statement):
        vectors.append(vector(vector_id, shows, payload, expected))
    ids = [entry["id"] for entry in vectors]
    if len(ids) != len(set(ids)):
        raise SystemExit("vector ids must be unique")
    document = {
        "expected_environment": ENVIRONMENT,
        "expected_measurements": {"PCR0": PCR0_VALUE, "PCR1": PCR1_VALUE, "PCR2": PCR2_VALUE},
        "vectors": vectors,
    }
    json.dump(document, sys.stdout, indent=2)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
