#! /bin/bash

export REQUESTS_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt

# tenant id: cfd26XXXXXXXXXXXXXXXXXXXXXX15d884
# client id: 3d5c2XXXXXXXXXXXXXXXXXXXXXXXc63a13c3
# client-secret: 9Jp8XXXXXXXXXXXXXXXXXXXXXXXOi6wDIv-M-bBL

#az storage fs file upload --file-system "ingress" --auth-mode key --account-name "lzstrXXXXXXXXXXXXectupl" --account-key "Ml/5qXXXXXXXXXSt0uYPIw==" --path "/mrcan24/" --metadata owner=michael --source Carsten.txt

# az login --service-principal --username "3d5c2XXXXXXXXXXXXXXXXXXXXXXXc63a13c3" --password "9Jp8XXXXXXXXXXXXXXXXXXXXXXXOi6wDIv-M-bBL" --tenant "cfd26XXXXXXXXXXXXXXXXXXXXXX15d884"
# az storage fs file upload --file-system "ingress" --auth-mode login --blob-endpoint "https://lzstrXXXXXXXXXXXXectupl.dfs.core.windows.net" --overwrite --path "/mrcan24/Carsten2.txt" --metadata 'owner=michael' --source Carsten.txt
# az storage fs file upload --file-system "ingress" --auth-mode login --blob-endpoint "https://lzstrXXXXXXXXXXXXectupl.dfs.core.windows.net" --path "/mrcan24/Carsten.txt" --source Carsten.txt

az login --service-principal --username "fe72aXXXXXXXXXXXXXXXXXXXXXXXeceb36" --password "v~R8XXXXXXXXXXXXXXXXXXXXXXX9b5HwquU1aIJ" --tenant "cfd26XXXXXXXXXXXXXXXXXXXXXX15d884"
az storage fs file upload --file-system "mrcan24" --overwrite --auth-mode login --blob-endpoint "https://lzstrXXXXXXXXXXXXuplgld.dfs.core.windows.net" --metadata 'owner=michael' --path "/Carsten2.txt" --source Carsten.txt
