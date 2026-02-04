#!/bin/sh
if [ ! "${AEROSTRESS_INTERACTIVE}" = "true" ]; then
	./aerostress
	echo Sleeping now
fi
sleep infinity
