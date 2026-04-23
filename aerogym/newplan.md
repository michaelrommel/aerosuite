We now want to implement a new module and extend/change an existing one. In the file aerogym/ARCHITECTURE.md is a preliminary plan, how to modify the existing stress tester in aerogym/src with new functionality. Originally it was envisioned, that an external script starts all the agents based on the sourecode there and then an aggregator collects the test progress and visualizes it.

I have since then revised the strategy. I now would like to implement a new module called aerocoach. This module shall completely control the agents in terms of their load testing behaviour. The parts of aggregating the process (the previous aggregator descritption) shall be subsumed in this module.

The agents shall still be started by a script, because a role to start containers on Fargate is not considered to be added to an IMDS role running the aerocoach container. Also a Container Service, where we could control the desired capacity is always tied to a minimum capacity of one, so that would mean, that we always have this container running, if we need to stress test or not. If there is another way, feel free to propose.

If the container starts up, it shall contact the aerocoach. I do not think that services like bonjour are compatible with AWS. So I don't know what kind of autodiscover methods would be suitable for an AWS container environment. In the worst case, it is not a problem to start aerocoach first note down the internal IP address of that container and then give this as an environment parameter to the script starting the agents. That is how today the aerogym tester finds the target address of the FTP server. The communication between aerocoach and aerogym shall be gRPC as envisioned.

The frontend dashboard shall be in a new module aerotrack. This functionality shall be as described.

So now, we need to extend this architectural vision with defining the exerted load and distributing it over the running agents.

Load Structure
==============

The load shall be modeled after an infrastructure, that we have in use today. Here we know, in a time sequence, how many FTP connections are running. We also know the histogram distribution of file sizes, that are transferred. Thirdly there is an upper limit on the bandwidth, that we can use.

So a model would consist of:
- definition of percentages of file sizes, like [{bucket: small, sizea; }]

Cluster-Details:
| File Size Range | Percentage |
|--- | -: |
|  0-10 MB      | ( 58.0%)|  
|  10-50 MB     | ( 12.9%)|
|  50-100 MB    | (  8.7%)|
|  100-200 MB   | (  6.3%)|
|  200-500 MB   | (  5.2%)|
|  500-1000 MB  | (  4.0%)|
|  >= 1 GB      | (  4.9%)|


- test files can be generated on each agent for those buckets for re-use. Each agent shall create one file in each bucket with a random size, that lies in the range of the bucket. That way we have different file sizes across the agents, but each agent can start tens or hundreds of connections using the same file on disk. Saves disk space.
- each file transfer - although possibly reusing the same file - shall use different names when sending it to the backend. Each agent shall have one numbered identifier, like a00 to a99 (we will not use more than 100 containers). The file name then can consist of the time slice number, followed by a kind of connection/task id number, so that the filenames across an agent are unique in a time slice.
- definition of number of connections over time, looking like a bar graph, the interval between each step on the x-axis shall be definable and the y-axis shall be the number of concurrent connections across all agents
- definition of the bandwidth to be used. The bandwidth shall be defined as across all agents. So if we divide the connections across the agents, each agent shall get a portion of the bandwidth it can use. The existing source code already has a rate limiter implementation, that worked well in the past. With it we can set a chunk size and a time interval in which those chunks shall be sent, effectively giving use a bandwidth. 
- if a transfer is started in a time slice and the transfer is quick, because the file is small, then we will not start it again in this slice.
- The problem space is here: if the file size is large and the bandwidth available is low, the transfer will likely spill over into the next time slice. Since the distribution size of the files shall remain across the whole test, we probably can just calculate, what the current slice should have as load and use tokio::tasks to only start those new transfers that are not currently running.
- In each time slice we might conversely also to ramp down load. We should avoid terminating already running transfers from previous slices, as this would distort the number of failed vs. successful transfers.
- we want to record each transfer with number of bytes transferred, filename, bandwidth in kiBytes/second, success or error flag, error reason, we get from the backend.

Communication / Synchronization
===============================

- the load model can be calculated and distributed up front for the test
- if a parameter, like the bandwidth to be used shall be changed during the runtime of the test, an updated calculation can be sent down to the agents. In this case the agents should then change the slices from the current slice onwards. 
    - Bandwidth change: Usually this would mean just changing the rate limit parameters, keeping the files and number of connections intact.
    - File Distribution change: Since files for each bucket have been preallocated on disk, it would just change the assignment of connection tasks to different files.
    - Connection number change: if the load profile differs heavily, it will take time to adapt: ramping up additional connections is not a problem, we just start more tasks. having a sudden decreast of intended connections, means we still let the existing transfers finish, not shut them down. Even it that will not match the profile 100%.
- There needs to be some kind of synchronization, which slice each agent is on, so that they perform in lock-step.

Migration of existing code
==========================

We should move the two files config.rs and main.rs in aerogym into an own binary package, so that the old version can still be compiled and used separately.

Otherwise we can create a new codebase inside aerogym, aerocoach and aerotrack from scratch. The latter two directories do not yet exist.




