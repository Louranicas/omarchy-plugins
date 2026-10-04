#include "transport.hpp"
#include <cassert>
#include <iostream>
uint64_t selfStart(){int p=open("/proc/self",O_PATH|O_DIRECTORY|O_CLOEXEC);assert(p>=0);auto s=join_fixture::startTicks(p);close(p);return s;}
int main(){
 {
  int pair[2];assert(socketpair(AF_UNIX,SOCK_STREAM|SOCK_CLOEXEC,0,pair)==0);int size=1024;assert(setsockopt(pair[0],SOL_SOCKET,SO_SNDBUF,&size,sizeof size)==0);
  join_fixture::Sender sender(pair[0],getpid(),selfStart(),"{\"padding\":\""+std::string(1500,'a')+"\"");
  unsigned sent=0;while(sender.edge("down"))++sent;
  assert(sent>0 && sent<128);assert(!sender.edge("up"));
  char data[4096];assert(read(pair[1],data,sizeof data)>0);assert(!sender.edge("down"));close(pair[1]);
  std::cout<<"backpressure_poisoned_after="<<sent<<"\n";
 }
 {
  int pair[2];assert(socketpair(AF_UNIX,SOCK_STREAM|SOCK_CLOEXEC,0,pair)==0);
  join_fixture::Sender sender(pair[0],getpid(),selfStart(),"{");close(pair[1]);assert(!sender.edge("ready"));assert(!sender.edge("down"));
 }
 {
  int pair[2];assert(socketpair(AF_UNIX,SOCK_STREAM|SOCK_CLOEXEC,0,pair)==0);bool refused=false;
  try{join_fixture::Sender sender(pair[0],getpid(),selfStart()+1,"{");}catch(...){refused=true;}
  assert(refused);close(pair[1]);
 }
 std::cout<<"peer_loss_and_wrong_start_refused=true\n";
}
