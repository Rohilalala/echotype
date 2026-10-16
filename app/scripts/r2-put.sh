#!/bin/sh
# Uploads a file to the echotype-models R2 bucket (models, LICENSE, app.json), up to 5 GB in one PUT.
#   app/scripts/r2-put.sh <local file> [name in the bucket]      default name: the file's own
# The R2 API token's key pair is read from the Keychain item "echotype-r2" (account "r2"), saved once with:
#   read -s "?R2 Access Key ID: " K; echo; read -s "?R2 Secret Access Key: " S; echo
#   security add-generic-password -a r2 -s echotype-r2 -w "$K:$S" -U; unset K S
# Files are public at https://pub-da62fbe34de644598313554b0a5ccc51.r2.dev/<name>.
#
# Notices and retiring versions (src-tauri/src/notices.rs) live in app.json there:
#   { "min_version": "0.1.0",
#     "notices": [{ "id": "unique-id", "title": "…", "body": "…", "action_label": "Update", "action": "update",
#                   "min_version": "", "max_version": "" }] }
# action: "update", "page:<page>" (home, speech, cleanup, account, settings…) or an https:// link.
# Raise min_version to make every older copy stop until it updates; apps pick it up within 6 hours.
set -eu
FILE=$1
NAME=${2:-$(basename "$FILE")}
EP=https://d2585f102ae90bd0f33cc6cc72686baa.r2.cloudflarestorage.com/echotype-models
case $NAME in *.json) python3 -m json.tool "$FILE" >/dev/null || { echo "r2-put.sh: $FILE is not valid JSON" >&2; exit 1; } ;; esac
printf 'user = "%s"\n' "$(security find-generic-password -a r2 -s echotype-r2 -w)" |
  curl -K - --fail-with-body -sS --aws-sigv4 "aws:amz:auto:s3" -H "x-amz-content-sha256: UNSIGNED-PAYLOAD" \
    -T "$FILE" "$EP/$NAME" -o /dev/null -w "$NAME: HTTP %{http_code}, %{size_upload} bytes\n"
