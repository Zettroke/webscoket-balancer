const WebSocket = require('ws');
l = []
for (let i=0; i<10000; i++) {
	const ws = new WebSocket("ws://127.0.0.1:1337/sadsa/qwewqe?urewr=sadsa");
	//const ws = new WebSocket(`ws://127.0.0.1:133${i%2==0?8:9}/sadsa/qwewqe?urewr=sadsa`);
    ws.on('open', () => {
        ws.send("kappa");
    })

    l.push(ws);
		
		

}
