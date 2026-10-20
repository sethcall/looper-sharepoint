We need to decide how we start on this, but ideally we can support sharepoint is 1st, because it's permission model matches the 1st real customer usecase. So if we do sharepoint, we first need to de-risk the ability to sync the local share agressively.  We need a way to set up a sharepoint easily for QA.  The thing I'm skeptical of is reasonably low-lanecy syncing (1 minute would be the worst case time to sync, but ideally we can , initiate a sync programitaclly whenever we want, as the same cadence & latency as with git.   We want to make the sharepoint sync code be a standalone open source project.  


The code base shoudl be rust. 

We should search for any open source stuff we can find first before we do this.  We have to ultimately be windows and mac compatible.  We actually have a windows machine on the same network, at sethc@seth-windows, you can ssh into and potentially drive effort on,( or I can drive using git syncing to that machine, just depends on how much windows 'low level' code we need to work on, and if it's worth this.

We want this to be enterprise friendly.   By that we just mean, somehow the enterprise, eventually can control the content copied from sharepoint.   Onedrive probably (I assume) automatically deleetes content when the source in sharepoint is gone ,we should put on our roadmap the same level of compliance.


