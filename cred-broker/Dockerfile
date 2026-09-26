# 单进程、单职责：一个 HTTP 接口，监听 80（compose 里用 PORT=80 + 非特权 sysctl 落地）。没有后台任务、没有静态站点。
#
# 构建上下文 = 本项目目录（docker-compose.yml 里 context: .）。
# 镜像里只需要依赖与 src/：db/schema.mysql.sql、test/、scripts/ 都是宿主机侧的东西，
# 由 .dockerignore 挡在外面 —— 尤其 node_modules（darwin/arm64 装的东西在 alpine 里跑不起来）
# 与 .env（本项目存的就是别人的凭证，库密码一次都不能进镜像）。
#
# 不写 `# syntax=` 指令：这台机器拉不到外部的 dockerfile frontend 镜像，
# 内置 frontend 已支持本文件用到的全部语法。
FROM node:24-alpine

ENV NODE_ENV=production
WORKDIR /app

# 依赖单独一层：只改源码的重复构建不会重新 npm ci
COPY package.json package-lock.json ./
RUN npm ci --omit=dev

COPY src/ ./src/

# 不 chown：代码与依赖归 root 只读，进程（node 用户）写不到自己的二进制里去。
# 运行时唯一需要写的地方是 MySQL。
USER node
EXPOSE 80

# 不用 docker --init / tini：src/server.js 自己装了 SIGTERM 处理
# （先停接单、等在途请求收尾再关连接池），node 作为 PID 1 收到 docker stop 就走这条路。
CMD ["node", "src/server.js"]
