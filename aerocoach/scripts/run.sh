#!/bin/sh
if [ ! "${AEROCOACH_INTERACTIVE}" = "true" ]; then
	./aerocoach
	echo Sleeping now
fi
sleep infinity
