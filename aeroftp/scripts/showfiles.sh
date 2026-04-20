#! /bin/bash

export REQUESTS_CA_BUNDLE=/etc/ssl/certs/zscaler_root.pem
az storage fs file list --file-system "ingress" --auth-mode key --account-name "lzstrXXXXXXXXXXXXectupl" --account-key "Ml/5qXXXXXXXXXSt0uYPIw==" --recursive false --path "/mrcan24/"

# client id: d5c23b8-6247-4572-848e-e8b5c63a13c3
# client-secret: 9Jp8XXXXXXXXXXXXXXXXXXXXXXXOi6wDIv-M-bBL
