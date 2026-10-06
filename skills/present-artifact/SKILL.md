---
name: present-artifact
description: Show the user an on-disk artifact (a generated image, chart, screenshot, or other file) inline in the conversation WITHOUT loading it into the model's context. Use when the user should see a result before it is exported, committed, or approved; use attach-image instead when you need to see an image yourself.
---

# Present Artifact

Present an on-disk artifact in the current Prime Agent conversation:

```python
await present_artifact("/path/to/chart.png")
await present_artifact("out/render.png", label="Direction A")
```

This is a display-only action for the user. It does not load the artifact into
your context, upload it, copy it into a repository, or imply approval. The host
captures the file under the session's artifact directory, so the source may be
removed after the call returns.

The call returns a compact receipt: `artifactId`, `presentationId`, `kind`
(`image` or `file`), `name`, `mimeType`, `byteSize`, the captured `path`, and,
for images, the preview `width`/`height` and `originalWidth`/`originalHeight`.

Raster images (PNG, JPEG, GIF, WebP) get a bounded inline preview (at most
1600x1600 and about 260KB) that travels with the conversation; other files are
captured with their metadata and the captured path. Missing paths, directories,
and files over 20 MiB raise an exception.
