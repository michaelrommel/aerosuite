#!/usr/bin/env bash

USERNAME=$1
PASSWORD=$2

iter=500000
salt=$(dd 2>/dev/null if=/dev/random bs=1 count=8)
PBKDF_KEY=$(echo -n "$PASSWORD" | nettle-pbkdf2 -i $iter -l 32 --hex-salt "$(echo -n $salt | xxd -p -c 80)" --raw | openssl base64 -A)
PBKDF_SALT=$(echo -n $salt | openssl base64 -A)

cat <<EOF
{
	"username": "$USERNAME",
	"pbkdf2_salt": "$PBKDF_SALT",
	"pbkdf2_key": "$PBKDF_KEY",
	"pbkdf2_iter": $iter
}
EOF
