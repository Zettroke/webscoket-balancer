const WebSocket = require('ws');
l = []

const timeout = function (t) {
    return new Promise((resolve) => setTimeout(resolve, t));
};



async function f() {
    l.push(new WebSocket(`ws://127.0.0.1:1337/${Math.random()}/qwewqe?roomId=sadsa8`));
    await timeout(500);
    l.push(new WebSocket(`ws://127.0.0.1:1337/${Math.random()}/qwewqe?roomId=sadsa9`));
    await timeout(500);
    for (let i=0; i<20000; i++) {
        const ws = new WebSocket(`ws://127.0.0.1:1337/${Math.random()}/qwewqe?roomId=sadsa${i%2==0?8:9}`);
        // const ws = new WebSocket(`ws://127.0.0.1:133${i%2==0?8:9}/sadsa/qwewqe?urewr=sadsa`);
        // ws.on('open', () => {
        //     ws.send("kappa");
        // })

        l.push(ws);

        // await timeout(1);s
    }
}

f();


