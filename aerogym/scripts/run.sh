#!/bin/sh
if [ ! "${AEROSTRESS_INTERACTIVE}" = "true" ]; then
	./aerogym
	# for now exit immediately. Need to clean up lots of tasks...
	exit 0
	echo Sleeping now
fi
sleep infinity
