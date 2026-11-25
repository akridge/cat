# Local CAT server for desktop development

The desktop app talks to the unchanged CAT server (`cat/`). To test sign-in,
projects and sync without touching Dev or Prod, run Oracle Free and the
server locally.

```bash
# 1. Oracle Free with the repo's init scripts
docker run -d --name cat-oracle -p 1521:1521 \
  -e ORACLE_PASSWORD=CatDev123 -e APP_USER=cat_user -e APP_USER_PASSWORD=CatDev123 \
  -v "$PWD/scripts/db-init:/container-entrypoint-initdb.d:ro" \
  gvenzl/oracle-free:slim-faststart
docker logs -f cat-oracle   # wait for "DATABASE IS READY TO USE!"

# 2. CAT server (from the repo root, in a virtualenv)
python -m venv .venv && .venv/bin/pip install -e .
export CAT_STORAGE_BACKEND=oracle CAT_MODE=db \
       CAT_DB_USER=cat_user CAT_DB_PASSWORD=CatDev123 CAT_DB_DSN=localhost:1521/FREEPDB1 \
       CAT_DB_AUTO_BOOTSTRAP=true \
       CAT_AUTH_BOOTSTRAP_ADMIN_USERNAME=admin CAT_AUTH_BOOTSTRAP_ADMIN_EMAIL=admin@example.com \
       CAT_AUTH_BOOTSTRAP_ADMIN_PASSWORD=CatDev12345
.venv/bin/python -m uvicorn cat.server:app --host 127.0.0.1 --port 8000

# 3. A project whose image is a bucket COG
curl -c /tmp/c -H 'Content-Type: application/json' -d '{"username":"admin","password":"CatDev12345"}' http://127.0.0.1:8000/api/auth/login
curl -b /tmp/c -H 'Content-Type: application/json' -d '{"project_name":"2026_KIN-5151","site":"KIN-5151","year":2026}' http://127.0.0.1:8000/api/db/projects
curl -b /tmp/c -H 'Content-Type: application/json' \
  -d '{"asset_name":"2026_KIN-5151_mos_cog.tif","cog_url":"gs://nmfs_odp_pifsc/PIFSC/ESD/ARP/StRS_Sites_Products/orthomosaic_cog/2026/2026_KIN-5151_mos_cog.tif"}' \
  http://127.0.0.1:8000/api/db/projects/1/assets
```

Then in CAT.exe: click **Local mode · Sign in** (top right). Use name `Dev`,
URL `http://127.0.0.1:8000`, user `admin`.

Desktop requirements on a deployed server:

- The desktop must reach the server URL directly over HTTPS. A server that is
  only reachable through Google Cloud Workstations' browser login cannot be
  used from the desktop. Use a URL that doesn't need a browser sign-in.
- Project images must be bucket COGs (`gs://…`, `/vsigs/…` or `https://…`).
  Files on the server's own disk can't be streamed to the desktop.
