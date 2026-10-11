# Echotype Cloud server. Pure Go (modernc SQLite), so the binary is static.
FROM golang:1.26-alpine AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /out/server . && mkdir -p /out/data

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /out/server /server
# Owned by the nonroot user so a fresh named volume mounted here is writable.
COPY --from=build --chown=65532:65532 /out/data /data
ENV PORT=8080 DATABASE_PATH=/data/echotype-cloud.db
EXPOSE 8080
USER nonroot:nonroot
ENTRYPOINT ["/server"]
